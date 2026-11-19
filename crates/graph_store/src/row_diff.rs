//! How two answers of the same read differ, row by row (spec 036): the rows one has and the
//! other lacks, and the rows both have whose other cells moved. A review's Effect compares a
//! read at the target's head with the same read at the branch's; a query tab compares a run
//! with the run before it.
//!
//! Plain data, no `gpui`: the work is done off the main thread, and a view draws its answer.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

/// The most rows of each side compared. A read answering more is compared on its first rows,
/// and says so ([`RowDiff::capped`]).
pub const COMPARED_ROWS: usize = 5_000;

/// How the rows of two answers were matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchedBy {
    /// By the value of one column, which names each row once on both sides: the node a row
    /// shows (a whole node's `@id`, or a column such as `i.slug`).
    Column(usize),
    /// By the whole row: a row is the same row only when every cell is.
    WholeRow,
}

/// One row both answers have, whose other cells differ.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowChange {
    pub before: usize,
    pub after: usize,
    /// The columns whose cells differ.
    pub columns: Vec<usize>,
}

/// What a second answer of a read has that the first had not ([`diff_rows`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowDiff {
    pub matched_by: MatchedBy,
    /// Rows of the second answer the first lacks, by their place in it.
    pub added: Vec<usize>,
    /// Rows of the first answer the second lacks, by their place in it.
    pub removed: Vec<usize>,
    /// Rows both have whose other cells moved (only when matched by a column).
    pub changed: Vec<RowChange>,
    /// One side had more than [`COMPARED_ROWS`] rows, and only its first were compared.
    pub capped: bool,
}

impl RowDiff {
    /// No row came, went or moved.
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }

    /// The counts in a few characters, as a row's end shows them: "+4 −1 ~1", the counts that
    /// are not zero; empty when nothing moved.
    pub fn counts(&self) -> String {
        let mut parts = Vec::new();
        if !self.added.is_empty() {
            parts.push(format!("+{}", self.added.len()));
        }
        if !self.removed.is_empty() {
            parts.push(format!("−{}", self.removed.len()));
        }
        if !self.changed.is_empty() {
            parts.push(format!("~{}", self.changed.len()));
        }
        parts.join(" ")
    }

    /// Whether the row at `index` of the second answer is new to it.
    pub fn added_at(&self, index: usize) -> bool {
        self.added.binary_search(&index).is_ok()
    }

    /// The change of the row at `index` of the second answer, when its cells moved.
    pub fn changed_at(&self, index: usize) -> Option<&RowChange> {
        self.changed.iter().find(|change| change.after == index)
    }
}

/// How `after` differs from `before`, two answers of the same read with the same `columns`.
///
/// Rows are matched by the node a row names when one column names each row once on both sides
/// ([`key_column`]): then a row in both whose other cells differ is changed, not removed and
/// added. Otherwise rows are matched whole, each row of one side used once. Each side is
/// compared on its first [`COMPARED_ROWS`] rows.
pub fn diff_rows(columns: &[String], before: &[Vec<Value>], after: &[Vec<Value>]) -> RowDiff {
    let capped = before.len() > COMPARED_ROWS || after.len() > COMPARED_ROWS;
    let before = &before[..before.len().min(COMPARED_ROWS)];
    let after = &after[..after.len().min(COMPARED_ROWS)];
    match key_column(columns, before, after) {
        Some(column) => {
            let place: HashMap<&str, usize> = before
                .iter()
                .enumerate()
                .filter_map(|(at, row)| Some((row_key(row.get(column)?)?, at)))
                .collect();
            let mut seen = HashSet::new();
            let mut added = Vec::new();
            let mut changed = Vec::new();
            for (at, row) in after.iter().enumerate() {
                let Some(key) = row.get(column).and_then(row_key) else {
                    added.push(at);
                    continue;
                };
                match place.get(key) {
                    Some(&was) => {
                        seen.insert(was);
                        let moved: Vec<usize> = (0..columns.len())
                            .filter(|&i| before[was].get(i) != row.get(i))
                            .collect();
                        if !moved.is_empty() {
                            changed.push(RowChange {
                                before: was,
                                after: at,
                                columns: moved,
                            });
                        }
                    }
                    None => added.push(at),
                }
            }
            let removed = (0..before.len()).filter(|at| !seen.contains(at)).collect();
            RowDiff {
                matched_by: MatchedBy::Column(column),
                added,
                removed,
                changed,
                capped,
            }
        }
        None => {
            // Each row of `before` matches one equal row of `after` at most.
            let mut left: HashMap<String, Vec<usize>> = HashMap::new();
            for (at, row) in before.iter().enumerate().rev() {
                left.entry(whole(row)).or_default().push(at);
            }
            let mut added = Vec::new();
            for (at, row) in after.iter().enumerate() {
                match left.get_mut(&whole(row)).and_then(Vec::pop) {
                    Some(_) => {}
                    None => added.push(at),
                }
            }
            let mut removed: Vec<usize> = left.into_values().flatten().collect();
            removed.sort_unstable();
            RowDiff {
                matched_by: MatchedBy::WholeRow,
                added,
                removed,
                changed: Vec::new(),
                capped,
            }
        }
    }
}

/// The column that names each row once on both sides, when there is one: the first that
/// returns whole nodes, else the first whose name ends in `@id`, `id`, `slug` or `key`, else
/// the first column, each taken only when every row of both sides holds a text (or a node's
/// `@id`) there and no two rows of one side hold the same. Without one, rows are matched
/// whole. The read's text is not known for a stored query (E43), so the names are all there
/// is to go by.
pub fn key_column(
    columns: &[String],
    before: &[Vec<Value>],
    after: &[Vec<Value>],
) -> Option<usize> {
    let names_each_once = |column: usize| {
        [before, after].iter().all(|rows| {
            let mut seen = HashSet::new();
            rows.iter().all(|row| {
                row.get(column)
                    .and_then(row_key)
                    .is_some_and(|key| seen.insert(key))
            })
        })
    };
    let holds_nodes = |column: usize| {
        before
            .iter()
            .chain(after)
            .next()
            .and_then(|row| row.get(column))
            .is_some_and(|value| value.get("@id").is_some())
    };
    let named = |column: &String| {
        let last = column.rsplit('.').next().unwrap_or(column);
        matches!(last, "@id" | "id" | "slug" | "key") || last.ends_with("_id")
    };
    let mut candidates: Vec<usize> = (0..columns.len()).filter(|&i| holds_nodes(i)).collect();
    candidates.extend((0..columns.len()).filter(|&i| named(&columns[i])));
    if !columns.is_empty() {
        candidates.push(0);
    }
    let mut tried = HashSet::new();
    candidates
        .into_iter()
        .filter(|column| tried.insert(*column))
        .find(|&column| names_each_once(column))
}

/// What names a row in a key column: a text, or a whole node's `@id`.
fn row_key(value: &Value) -> Option<&str> {
    match value {
        Value::String(text) => Some(text),
        Value::Object(node) => node.get("@id").and_then(Value::as_str),
        _ => None,
    }
}

/// A whole row as one comparable text.
fn whole(row: &[Value]) -> String {
    serde_json::to_string(row).unwrap_or_default()
}

/// A read's rows as the cells of its columns, in the server's column order: a row's missing
/// key reads as null, as the server leaves a null cell's key out.
pub fn cells(columns: &[String], rows: &Value) -> Vec<Vec<Value>> {
    rows.as_array()
        .map(|rows| {
            rows.iter()
                .map(|row| {
                    columns
                        .iter()
                        .map(|column| row.get(column).cloned().unwrap_or(Value::Null))
                        .collect()
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn rows(values: Value) -> Vec<Vec<Value>> {
        values
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row.as_array().unwrap().clone())
            .collect()
    }

    fn columns(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    /// beads B4: closing `lk-acl` makes three issues ready and takes `lk-acl` out.
    #[test]
    fn a_queue_says_which_rows_came_and_went() {
        let columns = columns(&["i.slug", "i.title", "i.priority"]);
        let main = rows(json!([
            ["lk-sync", "Offline sync", 0],
            ["lk-acl", "Per-notebook access", 1],
            ["lk-release", "Release 0.9", 1],
        ]));
        let branch = rows(json!([
            ["lk-sync", "Offline sync", 0],
            ["lk-share", "Shared notebooks", 1],
            ["lk-release", "Release 0.9", 1],
            ["lk-share-api", "Invite endpoint", 2],
        ]));
        let diff = diff_rows(&columns, &main, &branch);
        assert_eq!(diff.matched_by, MatchedBy::Column(0));
        assert_eq!(diff.added, vec![1, 3]);
        assert_eq!(diff.removed, vec![1]);
        assert!(diff.changed.is_empty());
        assert_eq!(diff.counts(), "+2 −1");
        assert!(diff.added_at(3) && !diff.added_at(2));
    }

    /// program P2: the roll-up keeps its rows, and one row's allocation and verdict move.
    #[test]
    fn a_roll_up_says_which_cells_moved() {
        let columns = columns(&["r.slug", "allocated", "violated"]);
        let main = rows(json!([
            ["dc-pl-mass", 30.8, true],
            ["dc-str-mass", 38.2, false]
        ]));
        let branch = rows(json!([
            ["dc-pl-mass", 27.7, false],
            ["dc-str-mass", 38.2, false]
        ]));
        let diff = diff_rows(&columns, &main, &branch);
        assert_eq!(
            diff.changed,
            vec![RowChange {
                before: 0,
                after: 0,
                columns: vec![1, 2]
            }]
        );
        assert_eq!(diff.counts(), "~1");
        assert!(diff.changed_at(0).is_some());
    }

    /// No column names each row once (a count, a repeated name): rows match whole, and two
    /// equal rows on one side are two rows.
    #[test]
    fn rows_without_a_naming_column_match_whole() {
        let columns = columns(&["status", "n"]);
        let main = rows(json!([["open", 3], ["open", 3], ["closed", 6]]));
        let branch = rows(json!([["open", 3], ["closed", 7]]));
        let diff = diff_rows(&columns, &main, &branch);
        assert_eq!(diff.matched_by, MatchedBy::WholeRow);
        assert_eq!(diff.added, vec![1]);
        assert_eq!(diff.removed, vec![1, 2]);
    }

    /// A column returning whole nodes names its rows by their `@id`, before any other.
    #[test]
    fn a_node_column_names_its_rows() {
        let columns = columns(&["title", "i"]);
        let main = rows(json!([["Same", {"@id": "a", "status": "open"}]]));
        let branch = rows(json!([
            ["Same", {"@id": "a", "status": "closed"}],
            ["Same", {"@id": "b", "status": "open"}]
        ]));
        let diff = diff_rows(&columns, &main, &branch);
        assert_eq!(diff.matched_by, MatchedBy::Column(1));
        assert_eq!(diff.added, vec![1]);
        assert_eq!(diff.changed.len(), 1);
    }

    #[test]
    fn a_long_answer_is_compared_on_its_first_rows() {
        let columns = columns(&["id"]);
        let many: Vec<Vec<Value>> = (0..COMPARED_ROWS + 10)
            .map(|i| vec![json!(format!("n{i}"))])
            .collect();
        let diff = diff_rows(&columns, &many, &many);
        assert!(diff.capped && diff.is_empty());
    }

    #[test]
    fn cells_follow_the_columns_and_read_a_missing_key_as_null() {
        let columns = columns(&["a", "b"]);
        let read = json!([{"a": 1, "b": 2}, {"a": 3}]);
        assert_eq!(cells(&columns, &read), rows(json!([[1, 2], [3, null]])));
    }
}
