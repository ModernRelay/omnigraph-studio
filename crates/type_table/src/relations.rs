//! A node view's relationships (spec 004's relationship columns, spec 048's relationship
//! fields): for each row, how many neighbours it has through a relationship and the first few
//! of them by name. A table reads them for the relationship columns it draws and a Cards tab
//! for the relationship fields its format shows; both hold a [`Relations`] and read through
//! [`RelationHost`], so the queries, the chunk, the budget and the second read of what a hub
//! cut short are one thing. The types and the reads were `TypeTable`'s.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use gpui::{Context, SharedString, Task};
use gq_language::{BuiltQuery, RelationshipGroup, SchemaModel, builder};
use graph_client::ReadError;
use graph_client::api::ReadOutput;
use serde_json::Value;

use crate::model::{RELATION_CHIPS_SHOWN, RELATION_SAMPLE_BUDGET, chips_that_fit, relation_sample};
use crate::peek::NEIGHBOUR_LIMIT;

/// Rows per neighbour read when a relationship column comes into view: a page's worth, as the
/// page reads do.
pub(crate) const RELATION_READ_CHUNK: usize = 100;

/// One row's neighbours through one relationship: how many, and the first few by id (as many
/// as the peek shows; the cell shows `RELATION_CHIPS_SHOWN`). `whole` when the row's whole list
/// was read, so these are its first ones and the peek needs no read.
pub(crate) struct Relation {
    pub(crate) count: Result<i64, String>,
    pub(crate) neighbours: NeighbourList,
    pub(crate) whole: bool,
}

impl Relation {
    /// What `available` pixels show of a row with `count` neighbours: the first of them that
    /// fit whole, by name, as chips, and what is said after them, "+N" for those left out or
    /// the count alone when no chip fits. The count stays in view; the chips give way, whole.
    pub(crate) fn chips(
        &self,
        count: i64,
        available: f32,
    ) -> (Vec<SharedString>, Option<SharedString>) {
        let labels: Vec<&str> = self
            .neighbours
            .iter()
            .take(RELATION_CHIPS_SHOWN)
            .map(|(id, label)| label.as_deref().unwrap_or(id))
            .collect();
        let fit = chips_that_fit(&labels, count, available);
        let more = count - fit as i64;
        (
            labels[..fit]
                .iter()
                .map(|label| SharedString::from(label.to_string()))
                .collect(),
            (more > 0).then(|| {
                SharedString::from(if fit == 0 {
                    studio_ui::group_count(count)
                } else {
                    format!("+{}", studio_ui::group_count(more))
                })
            }),
        )
    }
}

/// Neighbours by id, each with its label when the other type has one.
pub(crate) type NeighbourList = Vec<(String, Option<String>)>;

/// A `page_neighbours` answer by row: how many neighbours it lists for each, and the first
/// [`NEIGHBOUR_LIMIT`] of them, by id.
pub(crate) struct NeighbourLists(HashMap<String, (i64, NeighbourList)>);

impl NeighbourLists {
    pub(crate) fn read(rows: &Value) -> Self {
        let mut lists: HashMap<String, (i64, NeighbourList)> = HashMap::new();
        for (id, other, label) in relation_samples(rows) {
            let (listed, first) = lists.entry(id).or_default();
            *listed += 1;
            if first.len() < NEIGHBOUR_LIMIT as usize {
                first.push((other, label));
            }
        }
        Self(lists)
    }

    /// Rows `ids` split by whether the answer lists every neighbour `degrees` counts for them:
    /// those it does with their first neighbours, and the rest, in the order given. A list the
    /// read's limit cut short, or none, is not the row's.
    pub(crate) fn split(
        mut self,
        ids: Vec<String>,
        degrees: &HashMap<String, i64>,
    ) -> (Vec<(String, NeighbourList)>, Vec<String>) {
        let mut whole = Vec::new();
        let mut rest = Vec::new();
        for id in ids {
            let degree = degrees.get(&id).copied().unwrap_or(0);
            match self.0.remove(&id) {
                Some((listed, first)) if listed == degree => whole.push((id, first)),
                _ => rest.push(id),
            }
        }
        (whole, rest)
    }
}

/// Values by row id and column, looked up without copying either key: every cell drawn asks
/// for its own.
pub(crate) struct CellMap<V>(pub(crate) HashMap<String, HashMap<String, V>>);

impl<V> Default for CellMap<V> {
    fn default() -> Self {
        Self(HashMap::new())
    }
}

impl<V> CellMap<V> {
    pub(crate) fn get(&self, row: &str, column: &str) -> Option<&V> {
        self.0.get(row)?.get(column)
    }

    pub(crate) fn get_mut(&mut self, row: &str, column: &str) -> Option<&mut V> {
        self.0.get_mut(row)?.get_mut(column)
    }

    pub(crate) fn contains(&self, row: &str, column: &str) -> bool {
        self.get(row, column).is_some()
    }

    pub(crate) fn insert(&mut self, row: String, column: String, value: V) {
        self.0.entry(row).or_default().insert(column, value);
    }

    pub(crate) fn remove(&mut self, row: &str, column: &str) -> Option<V> {
        let cells = self.0.get_mut(row)?;
        let removed = cells.remove(column);
        if cells.is_empty() {
            self.0.remove(row);
        }
        removed
    }

    pub(crate) fn clear(&mut self) {
        self.0.clear();
    }
}

/// `(id, other, label)` from a `page_neighbours` result.
pub(crate) fn relation_samples(rows: &Value) -> Vec<(String, String, Option<String>)> {
    rows.as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| {
            Some((
                row.get("id")?.as_str()?.to_owned(),
                row.get("other")?.as_str()?.to_owned(),
                row.get("label").and_then(Value::as_str).map(str::to_owned),
            ))
        })
        .collect()
}

/// What a view holds of its rows' relationships: what was read, the reads out, and what was
/// asked for.
#[derive(Default)]
pub(crate) struct Relations {
    /// Each row's neighbours through each relationship, by row id and relationship key; read
    /// at the rows' commit.
    pub(crate) cells: CellMap<Relation>,
    /// Relationship reads out, each taking itself out when done; a reload drops them all.
    pub(crate) tasks: HashMap<u64, Task<()>>,
    next_task: u64,
    /// Rows and relationship keys whose read is out or done, so one scrolled back into view is
    /// not asked again.
    pub(crate) asked: HashSet<(String, String)>,
}

impl Relations {
    pub(crate) fn get(&self, row: &str, key: &str) -> Option<&Relation> {
        self.cells.get(row, key)
    }

    /// Whether row `row`'s relationship `key` was read.
    pub(crate) fn contains(&self, row: &str, key: &str) -> bool {
        self.cells.contains(row, key)
    }

    /// The id of a read about to go out.
    pub(crate) fn next_task(&mut self) -> u64 {
        self.next_task += 1;
        self.next_task
    }

    /// Lets go of everything read and asked for, and drops the reads out: the rows are read
    /// again, at another commit or as other rows.
    pub(crate) fn clear(&mut self) {
        self.cells.clear();
        self.tasks.clear();
        self.asked.clear();
    }
}

/// What the relationship reads ask of the view that holds them. The view keeps its rows and
/// says which of them to read ([`Self::read_relations_of`]); the reads fill its [`Relations`].
pub(crate) trait RelationHost: Sized + 'static {
    fn relations(&self) -> &Relations;
    fn relations_mut(&mut self) -> &mut Relations;

    /// The schema the rows are read under and the node type they are of. `None` before the
    /// schema is read, and for a view whose rows are not nodes (an edge table): nothing is
    /// read then.
    fn relation_rows(&self) -> Option<(Arc<SchemaModel>, &str)>;

    /// The commit the rows were read at, which their relationships are read at too.
    fn relation_pin(&self) -> Option<String>;

    /// Reads `query` at `snapshot` as the view reads its rows: through its read scope while
    /// it follows its branch.
    fn read_relations(
        &self,
        query: &BuiltQuery,
        snapshot: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<ReadOutput, ReadError>>;

    /// Counts or neighbours landed: the view draws them. A view that draws what lands
    /// together once says so here.
    fn relations_landed(&mut self, cx: &mut Context<Self>) {
        cx.notify();
    }

    /// Rows `rows` now show their neighbours through relationship `key`, read under
    /// `read_under`. A table checks that the schema still names their type the same way.
    fn neighbours_shown(
        &mut self,
        _read_under: &Arc<SchemaModel>,
        _key: &str,
        _rows: &[String],
        _cx: &mut Context<Self>,
    ) {
    }

    /// Reads relationship `group` of rows `ids`, [`RELATION_READ_CHUNK`] rows a read.
    fn read_relations_of(
        &mut self,
        group: &RelationshipGroup,
        ids: &[String],
        cx: &mut Context<Self>,
    ) {
        for chunk in ids.chunks(RELATION_READ_CHUNK) {
            self.load_relation_groups(vec![group.clone()], chunk.to_vec(), cx);
        }
    }

    /// Reads relationships `groups` of rows `ids`, at the rows' commit: per relationship, how
    /// many neighbours each row has, and the first few of each for its chips. A row whose
    /// relationship was read, or is being read, is not asked for again.
    fn load_relation_groups(
        &mut self,
        groups: Vec<RelationshipGroup>,
        ids: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        let Some((schema, type_name)) = self
            .relation_rows()
            .map(|(schema, type_name)| (schema, type_name.to_owned()))
        else {
            return;
        };
        if ids.is_empty() {
            return;
        }
        let snapshot = self.relation_pin();
        for group in groups {
            let key = builder::relationship_key(&group);
            // Rows whose relationship was read at this commit, or is being read, need no read.
            let ids: Vec<String> = ids
                .iter()
                .filter(|id| {
                    !self.relations().contains(id, &key)
                        && !self
                            .relations()
                            .asked
                            .contains(&((*id).clone(), key.clone()))
                })
                .cloned()
                .collect();
            if ids.is_empty() {
                continue;
            }
            self.relations_mut()
                .asked
                .extend(ids.iter().map(|id| (id.clone(), key.clone())));
            let Ok(degrees) = builder::page_degrees(&schema, &type_name, &group, &ids) else {
                continue;
            };
            let degrees = self.read_relations(&degrees, snapshot.clone(), cx);
            // The neighbours go out with the degrees, not after them: every neighbour of the
            // rows, by row then neighbour, up to the budget. Most pages' rows are all in it.
            // The degrees, in first, show the counts; the rows the answer holds whole show
            // their chips as it lands; the rows it cut short (a hub, or rows after one that
            // spent the budget) are read again, sized by their counts, so a hub never leaves
            // the rows after it bare.
            let first = builder::page_neighbours(
                &schema,
                &type_name,
                &group,
                &ids,
                RELATION_SAMPLE_BUDGET as u32,
            )
            .ok()
            .map(|query| self.read_relations(&query, snapshot.clone(), cx));
            let ids = ids.clone();
            let schema = schema.clone();
            let type_name = type_name.clone();
            let snapshot = snapshot.clone();
            let task_id = self.relations_mut().next_task();
            let task = cx.spawn(async move |this, cx| {
                // How many neighbours each row has. An answer whose rows cannot be read fails
                // the column's counts, as a failed read does, rather than counting such a row as
                // having none.
                let counts: Result<HashMap<String, i64>, String> = match degrees.await {
                    Ok(output) => graph_store::page_counts(&output.rows)
                        .map(|counts| {
                            counts
                                .into_iter()
                                .map(|(id, n)| (id, i64::try_from(n).unwrap_or(i64::MAX)))
                                .collect()
                        })
                        .ok_or_else(|| "The neighbour counts could not be read".to_owned()),
                    Err(error) => Err(error.to_string()),
                };
                let shown = this.update(cx, |this, cx| {
                    for id in &ids {
                        let count = match &counts {
                            Ok(counts) => Ok(counts.get(id).copied().unwrap_or(0)),
                            Err(error) => Err(error.clone()),
                        };
                        let relation = Relation {
                            count,
                            neighbours: Vec::new(),
                            whole: false,
                        };
                        this.relations_mut()
                            .cells
                            .insert(id.clone(), key.clone(), relation);
                    }
                    if counts.is_err() {
                        this.relations_mut().tasks.remove(&task_id);
                    }
                    this.relations_landed(cx);
                });
                let (Ok(counts), Ok(())) = (counts, shown) else {
                    return;
                };
                // The rows that show chips: each whole list in page order while the budget
                // lasts, none of more than a hub's.
                let (sampled, _) = relation_sample(&ids, &counts);
                let output = match first {
                    Some(read) if !sampled.is_empty() => read.await.ok(),
                    _ => None,
                };
                // Neither read nor wanted: the counts show alone, as a failed read leaves them.
                let Some(output) = output else {
                    this.update(cx, |this, _| this.relations_mut().tasks.remove(&task_id))
                        .ok();
                    return;
                };
                let (whole, rest) = NeighbourLists::read(&output.rows).split(sampled, &counts);
                let shown = this.update(cx, |this, cx| {
                    this.show_neighbours(&schema, &key, whole, cx);
                    if rest.is_empty() {
                        this.relations_mut().tasks.remove(&task_id);
                    }
                    this.relations_landed(cx);
                });
                if rest.is_empty() || shown.is_err() {
                    return;
                }
                let (rest, total) = relation_sample(&rest, &counts);
                let read =
                    builder::page_neighbours(&schema, &type_name, &group, &rest, total as u32)
                        .ok()
                        .and_then(|query| {
                            this.update(cx, |this, cx| {
                                this.read_relations(&query, snapshot.clone(), cx)
                            })
                            .ok()
                        });
                let output = match read {
                    Some(read) => read.await.ok(),
                    None => None,
                };
                this.update(cx, |this, cx| {
                    this.relations_mut().tasks.remove(&task_id);
                    // A row this read does not hold whole either shows its count alone.
                    if let Some(output) = output {
                        let (whole, _) = NeighbourLists::read(&output.rows).split(rest, &counts);
                        this.show_neighbours(&schema, &key, whole, cx);
                        this.relations_landed(cx);
                    }
                })
                .ok();
            });
            self.relations_mut().tasks.insert(task_id, task);
        }
    }

    /// Shows the chips of rows whose whole lists of neighbours through relationship `key` were
    /// read under `read_under`; their counts show already. The host is told
    /// ([`Self::neighbours_shown`]): a table reads their names again when the schema names
    /// their type otherwise since.
    fn show_neighbours(
        &mut self,
        read_under: &Arc<SchemaModel>,
        key: &str,
        whole: Vec<(String, NeighbourList)>,
        cx: &mut Context<Self>,
    ) {
        let rows: Vec<String> = whole.iter().map(|(id, _)| id.clone()).collect();
        for (id, neighbours) in whole {
            if let Some(relation) = self.relations_mut().cells.get_mut(&id, key) {
                relation.neighbours = neighbours;
                relation.whole = true;
            }
        }
        self.neighbours_shown(read_under, key, &rows, cx);
    }
}
