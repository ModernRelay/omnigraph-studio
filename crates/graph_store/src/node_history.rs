//! One node's history (spec 046): the commits of a branch's recent first-parent history that
//! changed the node or one of its relationships, newest first, each with who made it and how;
//! who last set each property (blame); and runs of one actor's small edits folded into one.
//!
//! Omnigraph has no read of the commits that changed one entity (E62). Studio reads the node
//! itself instead: the commit listing names a window of the branch's newest
//! [`NODE_HISTORY_COMMITS`] first-parent commits, and at each of them, and at the parent of the
//! oldest, Studio reads the node with a point query at that commit's snapshot, the ids of its
//! edges through each relationship group, and the size and kind of each Blob. Two consecutive
//! reads that differ are the change the newer commit made. The cost follows the window and the
//! node's relationship groups, never what the node's type wrote: a load of 50,000 nodes of the
//! type in the window costs what any other commit costs. It is exact for properties, a value
//! changed and changed back included. An older window goes on from the listing's page token, for
//! the head the first window resolved (constitution II).
//!
//! The change feed (`GET /changes` with `type` and `op`) is the alternative where a type changes
//! rarely; it is not used here (spec 046, Decisions).

use std::collections::{BTreeMap, HashMap};
use std::ops::Range;

use gpui::{AsyncApp, Context, Task, WeakEntity};
use gq_language::{BuiltQuery, Direction, RelationshipGroup, ScalarKind, SchemaModel, builder};
use graph_client::api::{ChangeImageOutput, EntityKind, QueryRequest};
use graph_client::{BlobAt, BlobCell, BlobMeta, GraphClient, HistoricalGap, ReadError};
use serde_json::{Map, Value};

use crate::GraphSession;
use crate::changes::{FoldedChange, NetChange};
use crate::ledger::Asker;
use crate::scope::ReadScope;

/// First-parent commits a window of a node's history covers, with "Read the 200 commits before"
/// as the honest exit.
pub const NODE_HISTORY_COMMITS: u32 = 200;

/// Reads a window sends at once.
pub const NODE_HISTORY_READS_AT_ONCE: usize = 4;

/// Edges of one relationship group a read at one commit lists at most; past it the view says
/// what the group did there is not known.
pub const NODE_HISTORY_EDGES: u32 = 1_000;

/// Merges in a window whose branch Studio names, one commit read each: past them an entry says
/// "via merge of commit …".
pub const NODE_HISTORY_MERGE_NAMES: usize = 20;

/// Commits the search for when a value was set lists at most beyond the windows read, in pages
/// of 1,000 (`GET /commits`).
pub const NODE_HISTORY_SEARCH_COMMITS: usize = 20_000;

/// How far apart two edits of one run may be (10 minutes, Notion's version window), in Unix
/// microseconds.
pub const FOLD_WINDOW: i64 = 10 * 60 * 1_000_000;

/// A commit of the branch's first-parent history, as the listing named it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowCommit {
    pub id: String,
    pub parent: Option<String>,
    /// Unix microseconds.
    pub at: i64,
    pub actor: Option<String>,
    /// The branch the commit was made on (`main` where the listing names none).
    pub branch: String,
    /// The merged branch's head, for a merge.
    pub merged_parent: Option<String>,
}

impl WindowCommit {
    fn of(commit: &graph_client::api::CommitOutput) -> Self {
        Self {
            id: commit.graph_commit_id.clone(),
            parent: commit.parent_commit_id.clone(),
            at: commit.created_at,
            actor: commit.actor_id.clone(),
            branch: commit
                .graph_branch
                .clone()
                .unwrap_or_else(|| "main".to_owned()),
            merged_parent: commit.merged_parent_commit_id.clone(),
        }
    }
}

/// A Blob cell as a node's history compares it: by kind and size, never by its bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlobState {
    Null,
    Stored { size: u64 },
    External { uri: String },
}

impl BlobState {
    fn of(meta: BlobMeta) -> Self {
        match meta {
            BlobMeta::Null => Self::Null,
            BlobMeta::Managed { size, .. } => Self::Stored { size },
            BlobMeta::External { uri } => Self::External { uri },
        }
    }

    /// The cell as an image holds it, which a card says as "Blob · 1.2 KB": its size, or its
    /// address.
    pub fn value(&self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Stored { size } => Value::String(size_words(*size)),
            Self::External { uri } => Value::String(uri.clone()),
        }
    }
}

/// `1.2 KB`, as Studio writes sizes (`studio_ui::format_bytes`, a layer up).
fn size_words(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1000.0;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// The node as one commit holds it: whether it exists, its properties, its Blobs and its edges
/// through each relationship group (by the group's index), each edge's id with its other end.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NodeState {
    pub exists: bool,
    pub properties: Map<String, Value>,
    pub blobs: BTreeMap<String, BlobState>,
    pub edges: BTreeMap<usize, BTreeMap<String, String>>,
    /// The groups whose read at this commit listed [`NODE_HISTORY_EDGES`] edges: only the first
    /// of them, by id, so an edge added or removed may push another past the page.
    pub full: std::collections::BTreeSet<usize>,
}

/// What a node's history reads at each commit: the node's type and id, its relationship groups
/// and its Blob properties.
#[derive(Debug, Clone)]
pub struct NodeScope {
    pub type_name: String,
    pub id: String,
    groups: Vec<RelationshipGroup>,
    blobs: Vec<String>,
}

impl NodeScope {
    /// The node `id` of `type_name`, with every relationship group and Blob property the schema
    /// gives its type.
    pub fn new(schema: &SchemaModel, type_name: &str, id: &str) -> Self {
        let blobs = schema
            .node(type_name)
            .map(|node| {
                node.properties
                    .iter()
                    .filter(|p| p.kind == ScalarKind::Blob)
                    .map(|p| p.name.clone())
                    .collect()
            })
            .unwrap_or_default();
        Self {
            type_name: type_name.to_owned(),
            id: id.to_owned(),
            groups: schema.relationships_of(type_name).to_vec(),
            blobs,
        }
    }

    /// Reads at one commit: the node, one per relationship group, one per Blob property.
    pub fn reads_per_commit(&self) -> usize {
        1 + self.groups.len() + self.blobs.len()
    }

    /// What changed from `older` to `newer` in `commit`: the node itself, and each edge added or
    /// removed.
    pub fn change(
        &self,
        older: &NodeState,
        newer: &NodeState,
        commit: &str,
    ) -> (Option<FoldedChange>, Vec<FoldedChange>) {
        let image = |state: &NodeState| {
            state.exists.then(|| {
                let mut properties = state.properties.clone();
                for (name, blob) in &state.blobs {
                    properties.insert(name.clone(), blob.value());
                }
                ChangeImageOutput {
                    properties,
                    endpoints: None,
                }
            })
        };
        let node = FoldedChange {
            kind: EntityKind::Node,
            type_id: self.type_name.clone(),
            type_name: self.type_name.clone(),
            id: self.id.clone(),
            before: image(older),
            after: image(newer),
            commits: vec![commit.to_owned()],
        };
        let node = node.net().is_some().then_some(node);
        let mut edges = Vec::new();
        let empty = BTreeMap::new();
        for (ix, group) in self.groups.iter().enumerate() {
            // A page cut at its limit says nothing exact of what changed: the group is too large
            // to follow, and the view says so in place of edges that may not have changed.
            if older.full.contains(&ix) || newer.full.contains(&ix) {
                continue;
            }
            let was = older.edges.get(&ix).unwrap_or(&empty);
            let now = newer.edges.get(&ix).unwrap_or(&empty);
            let image = |other: &str| {
                let (from, to) = match group.direction {
                    Direction::Outgoing => (self.id.as_str(), other),
                    Direction::Incoming => (other, self.id.as_str()),
                };
                ChangeImageOutput {
                    properties: Map::new(),
                    endpoints: Some(serde_json::json!({ "from": from, "to": to })),
                }
            };
            let edge = |id: &str, before, after| FoldedChange {
                kind: EntityKind::Edge,
                type_id: group.edge.clone(),
                type_name: group.edge.clone(),
                id: id.to_owned(),
                before,
                after,
                commits: vec![commit.to_owned()],
            };
            for (id, other) in now.iter().filter(|(id, _)| !was.contains_key(*id)) {
                edges.push(edge(id, None, Some(image(other))));
            }
            for (id, other) in was.iter().filter(|(id, _)| !now.contains_key(*id)) {
                edges.push(edge(id, Some(image(other)), None));
            }
        }
        (node, edges)
    }
}

/// One commit's changes to the node and its relationships.
#[derive(Debug, Clone)]
pub struct NodeHistoryEntry {
    pub commit: String,
    pub actor: Option<String>,
    /// When the commit was made, Unix microseconds.
    pub at: i64,
    /// The branch the commit was made on.
    pub branch: String,
    /// The merged branch's head, for a merge: the change came in with it, made there by others
    /// (E62).
    pub merged_parent: Option<String>,
    /// The branch the merged head was made on, once read: "via merge of …".
    pub merged_branch: Option<String>,
    /// What the commit did to the node itself.
    pub node: Option<FoldedChange>,
    /// What it did to the node's relationships.
    pub edges: Vec<FoldedChange>,
}

impl NodeHistoryEntry {
    /// The commit created the node.
    pub fn created(&self) -> bool {
        self.node
            .as_ref()
            .is_some_and(|node| node.net() == Some(NetChange::Added))
    }

    /// The commit removed the node, and with it its relationships.
    pub fn removed(&self) -> bool {
        self.node
            .as_ref()
            .is_some_and(|node| node.net() == Some(NetChange::Removed))
    }

    pub fn is_merge(&self) -> bool {
        self.merged_parent.is_some()
    }

    /// The commit was made on an agent's branch (`agent/<stamp>`, spec 013): the agent's own
    /// credential is not recorded yet (E20), so the branch is what says so.
    pub fn by_agent(&self) -> bool {
        self.branch.starts_with("agent/")
    }
}

/// Where an older window of a node's history goes on.
#[derive(Debug, Clone, PartialEq)]
pub struct OlderWindow {
    /// The commit listing's continuation, for the head the first window resolved: its page names
    /// the older window's commits.
    pub page_token: String,
    /// The node at the newest commit of the older window, read already as the window above's
    /// base.
    pub base: Option<(String, NodeState)>,
}

/// What a window of a node's history reads: the first, from the branch head, or an older one,
/// below the windows read.
#[derive(Debug, Clone, PartialEq)]
pub enum NodeHistoryWindow {
    Newest,
    Older {
        head: String,
        window: Box<OlderWindow>,
    },
}

/// A schema change between two commits that a read at the older one could not cross (E52): the
/// history stops at `commit`, the newer of the two.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaBoundary {
    pub commit: String,
    pub type_name: Option<String>,
}

/// A window of a node's history, as it was read.
#[derive(Debug, Clone)]
pub struct NodeHistoryPage {
    /// The head the window is read at: the branch's head when the first window was read.
    pub head: String,
    /// The node at the window's newest commit: for the first window, the node now.
    pub newest: Option<NodeState>,
    /// The node at the oldest commit read, and that commit: where a search past the windows
    /// starts.
    pub oldest: Option<(String, NodeState)>,
    /// The commits that changed the node or its relationships, newest first.
    pub entries: Vec<NodeHistoryEntry>,
    /// The window's commits whose change is known, newest first.
    pub commits: Vec<WindowCommit>,
    /// Where an older window goes on; `None` once the window reached the graph's first commit or
    /// a schema boundary, or stopped short.
    pub older: Option<OlderWindow>,
    /// Why the window was not read to its end, in words, when it was not.
    pub stopped: Option<String>,
    pub boundary: Option<SchemaBoundary>,
    /// Relationship groups with [`NODE_HISTORY_EDGES`] edges at some commit: what they did past
    /// those is not known.
    pub full_groups: Vec<RelationshipGroup>,
    /// Requests sent, for the end row and the validation log.
    pub requests: usize,
}

/// The window's entries, newest first: `states[i]` is the node at `commits[i]`, and
/// `states[commits.len()]` at the oldest commit's parent (none for the graph's first commit,
/// before which the node did not exist).
pub fn entries_of(
    scope: &NodeScope,
    commits: &[WindowCommit],
    states: &[NodeState],
) -> Vec<NodeHistoryEntry> {
    let nothing = NodeState::default();
    commits
        .iter()
        .enumerate()
        .filter_map(|(ix, commit)| {
            let newer = states.get(ix)?;
            let older = states.get(ix + 1).unwrap_or(&nothing);
            let (node, edges) = scope.change(older, newer, &commit.id);
            (node.is_some() || !edges.is_empty()).then(|| NodeHistoryEntry {
                commit: commit.id.clone(),
                actor: commit.actor.clone(),
                at: commit.at,
                branch: commit.branch.clone(),
                merged_parent: commit.merged_parent.clone(),
                merged_branch: None,
                node,
                edges,
            })
        })
        .collect()
}

/// Who last set one property, within what was read ("Now", spec 046).
#[derive(Debug, Clone, PartialEq)]
pub struct Blame {
    pub property: String,
    /// The value now, from the node at the head; `None` when the node is not there.
    pub value: Option<Value>,
    pub set: BlameSet,
}

/// How a property's value came to be, as far as the history read reaches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlameSet {
    /// Entry `entry` (an index into the entries, newest first) set it; `created` when it was the
    /// node's creation.
    In { entry: usize, created: bool },
    /// A search of point reads past the windows found this commit setting the value: the newest
    /// commit before which it held another (not exact for a value that went back to an earlier
    /// one).
    Found(WindowCommit),
    /// No commit read set it: it was set at or before this commit, the oldest read.
    Before(WindowCommit),
}

/// The node's properties (`properties`, the schema's order) as `now` holds them, each with the
/// newest entry of `entries` (newest first) that set it: an update whose images differ there, or
/// the node's creation, which sets every property. Past them, what a search `found`, else
/// "before" `bound`.
pub fn blame(
    entries: &[NodeHistoryEntry],
    properties: &[String],
    now: Option<&NodeState>,
    found: &HashMap<String, WindowCommit>,
    bound: Option<&WindowCommit>,
) -> Vec<Blame> {
    properties
        .iter()
        .filter_map(|name| {
            let set = entries.iter().enumerate().find_map(|(ix, entry)| {
                let node = entry.node.as_ref()?;
                let before = node.before.as_ref().map(|i| i.properties.get(name));
                let after = node.after.as_ref().map(|i| i.properties.get(name));
                match (before, after) {
                    (None, Some(_)) => Some(BlameSet::In {
                        entry: ix,
                        created: true,
                    }),
                    (Some(b), Some(a)) if b != a => Some(BlameSet::In {
                        entry: ix,
                        created: false,
                    }),
                    _ => None,
                }
            });
            let set = match (set, found.get(name), bound) {
                (Some(set), ..) => set,
                (None, Some(found), _) => BlameSet::Found(found.clone()),
                (None, None, Some(bound)) => BlameSet::Before(bound.clone()),
                (None, None, None) => return None,
            };
            let value = now.filter(|now| now.exists).map(|now| {
                now.blobs
                    .get(name)
                    .map(BlobState::value)
                    .or_else(|| now.properties.get(name).cloned())
                    .unwrap_or(Value::Null)
            });
            Some(Blame {
                property: name.clone(),
                value,
                set,
            })
        })
        .collect()
}

/// The runs of `entries` (newest first) that fold into one: consecutive entries by the same
/// actor on the same branch, of the same kind of author (`studio`, one flag an entry: Studio made
/// it this session), each less than [`FOLD_WINDOW`] after the one before, none a merge, a
/// creation or a removal. Every entry is in one run; a run of one is an entry on its own.
pub fn runs(entries: &[NodeHistoryEntry], studio: &[bool]) -> Vec<Range<usize>> {
    let foldable = |e: &NodeHistoryEntry| !e.is_merge() && !e.created() && !e.removed();
    let mut runs: Vec<Range<usize>> = Vec::new();
    for (ix, entry) in entries.iter().enumerate() {
        if let Some(run) = runs.last_mut()
            && ix > 0
        {
            let newer = &entries[ix - 1];
            let joins = foldable(entry)
                && foldable(newer)
                && entry.actor == newer.actor
                && entry.branch == newer.branch
                && studio.get(ix) == studio.get(ix - 1)
                && newer.at - entry.at < FOLD_WINDOW;
            if joins {
                run.end = ix + 1;
                continue;
            }
        }
        runs.push(ix..ix + 1);
    }
    runs
}

/// What a run of entries (newest first) did, net: the node from the oldest image before to the
/// newest after, and each relationship likewise; a change that ends where it began is left out.
pub fn net_of(run: &[NodeHistoryEntry]) -> (Option<FoldedChange>, Vec<FoldedChange>) {
    let commits: Vec<String> = run.iter().rev().map(|e| e.commit.clone()).collect();
    let fold = |changes: Vec<&FoldedChange>| -> Option<FoldedChange> {
        let (newest, oldest) = (changes.first()?, changes.last()?);
        let folded = FoldedChange {
            before: oldest.before.clone(),
            after: newest.after.clone(),
            commits: commits.clone(),
            ..(*newest).clone()
        };
        folded.net().is_some().then_some(folded)
    };
    let node = fold(run.iter().filter_map(|e| e.node.as_ref()).collect());
    let mut order: Vec<(String, String)> = Vec::new();
    let mut by_edge: HashMap<(String, String), Vec<&FoldedChange>> = HashMap::new();
    for edge in run.iter().flat_map(|e| e.edges.iter()) {
        let key = (edge.type_id.clone(), edge.id.clone());
        if !by_edge.contains_key(&key) {
            order.push(key.clone());
        }
        by_edge.entry(key).or_default().push(edge);
    }
    let edges = order
        .into_iter()
        .filter_map(|key| fold(by_edge.remove(&key)?))
        .collect();
    (node, edges)
}

/// The node's last change, as the session keeps it once its history was read (spec 046): a
/// node's page shows it in its meta line, and reads nothing for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastChange {
    /// The head the history was read at: the page shows the line only when it shows that commit.
    pub head: String,
    pub commit: String,
    pub actor: Option<String>,
    /// Unix microseconds.
    pub at: i64,
}

/// What the search past the windows found: the commit that created the node, and for each
/// property it was asked about the commit that set its value now.
#[derive(Debug, Clone, Default)]
pub struct SearchFound {
    pub created: Option<WindowCommit>,
    pub set: HashMap<String, WindowCommit>,
    /// Requests sent: listing pages and point reads.
    pub requests: usize,
    /// The oldest commit listed, when the listing stopped at its cap before the graph's first:
    /// an answer that still held there was set before it, which is all the search can say.
    pub before: Option<WindowCommit>,
    /// Why it stopped short, when it did.
    pub stopped: Option<String>,
}

/// The next step of a binary search over `0..len` for the oldest index a question holds at, it
/// holding at `lo` and not at `hi` (or `hi == len`): the index to ask about, or `None` once
/// `lo` is the answer.
pub fn search_step(lo: usize, hi: usize) -> Option<usize> {
    (hi - lo > 1).then(|| lo + (hi - lo) / 2)
}

/// What a binary search over `len` listed commits found, its answer holding at `lo`: that
/// commit, unless it is the oldest listed and the listing stopped short of the graph's first
/// commit (`reached_first` false), where the answer may hold further back still.
pub fn settled(lo: usize, len: usize, reached_first: bool) -> Option<usize> {
    (lo + 1 < len || reached_first).then_some(lo)
}

/// Who asks, through which client, of which graph and branch: what each read of a window needs.
struct Reader {
    session: WeakEntity<GraphSession>,
    asker: Asker,
    client: GraphClient,
    graph: String,
    branch: String,
    schema: std::sync::Arc<SchemaModel>,
    node: NodeScope,
    requests: usize,
}

/// What one read of a commit was for.
#[derive(Debug, Clone, Copy)]
enum Read {
    Node,
    Edges(usize),
    Blob(usize),
}

/// What one read of a commit answered.
enum Answer {
    Node(Option<Map<String, Value>>),
    Edges(usize, BTreeMap<String, String>, bool),
    Blob(usize, BlobState),
}

impl Reader {
    /// Starts `request` through the session, counted and named in its ledger as `what`.
    fn start<T: Send + 'static>(
        &mut self,
        what: &'static str,
        request: impl Future<Output = Result<T, ReadError>> + Send + 'static,
        cx: &mut AsyncApp,
    ) -> Task<Result<T, ReadError>> {
        self.requests += 1;
        let asker = self.asker;
        self.session
            .update(cx, |session, cx| {
                session.counted_read_by(what, asker, request, cx)
            })
            .unwrap_or_else(|_| {
                Task::ready(Err(ReadError::Transport("Studio closed the graph".into())))
            })
    }

    /// One query at `commit`'s snapshot, its rows.
    fn query(
        &mut self,
        what: &'static str,
        query: BuiltQuery,
        commit: &str,
        cx: &mut AsyncApp,
    ) -> Task<Result<Value, ReadError>> {
        let request = QueryRequest {
            query: query.text.clone(),
            name: Some(query.name.clone()),
            params: query.params_value(),
            branch: None,
            snapshot: Some(commit.to_owned()),
        };
        let (client, graph) = (self.client.clone(), self.graph.clone());
        self.start(
            what,
            async move {
                let output = client.query(&graph, &request).await?;
                Ok(std::sync::Arc::unwrap_or_clone(output.rows))
            },
            cx,
        )
    }

    /// Starts read `read` at `commit`; the answer is read where it is decoded.
    fn read_at(
        &mut self,
        read: Read,
        commit: &str,
        cx: &mut AsyncApp,
    ) -> Task<Result<Answer, ReadError>> {
        match read {
            Read::Node => {
                let query = match builder::history_node(
                    &self.schema,
                    &self.node.type_name,
                    &self.node.id,
                ) {
                    Ok(query) => query,
                    Err(error) => {
                        return Task::ready(Err(ReadError::Decode(format!("{error:?}"))));
                    }
                };
                let read = self.query("node_history_node", query, commit, cx);
                cx.background_executor().spawn(async move {
                    let rows = read.await?;
                    let node = rows
                        .as_array()
                        .and_then(|rows| rows.first())
                        .and_then(|row| row.get("n"))
                        .and_then(Value::as_object)
                        .map(|node| {
                            let mut node = node.clone();
                            node.remove("@id");
                            node
                        });
                    Ok(Answer::Node(node))
                })
            }
            Read::Edges(ix) => {
                let group = self.node.groups[ix].clone();
                let query = match builder::history_edges(
                    &self.schema,
                    &self.node.type_name,
                    &group,
                    &self.node.id,
                    NODE_HISTORY_EDGES,
                ) {
                    Ok(query) => query,
                    Err(error) => return Task::ready(Err(ReadError::Decode(format!("{error:?}")))),
                };
                let read = self.query("node_history_edges", query, commit, cx);
                cx.background_executor().spawn(async move {
                    let rows = match read.await {
                        Ok(rows) => rows,
                        // The relationship type did not exist at the commit: it had no edges.
                        Err(ReadError::Refused(refusal))
                            if matches!(
                                refusal.historical_gap(),
                                Some(HistoricalGap::Type { ref name }) if *name == group.edge
                            ) =>
                        {
                            Value::Array(Vec::new())
                        }
                        Err(error) => return Err(error),
                    };
                    let rows = rows.as_array().cloned().unwrap_or_default();
                    let full = rows.len() >= NODE_HISTORY_EDGES as usize;
                    let edges = rows
                        .iter()
                        .filter_map(|row| {
                            Some((
                                row.get("edge")?.as_str()?.to_owned(),
                                row.get("id")?.as_str()?.to_owned(),
                            ))
                        })
                        .collect();
                    Ok(Answer::Edges(ix, edges, full))
                })
            }
            Read::Blob(ix) => {
                let cell = BlobCell {
                    edge: false,
                    type_name: self.node.type_name.clone(),
                    id: self.node.id.clone(),
                    property: self.node.blobs[ix].clone(),
                };
                let (client, graph, commit) =
                    (self.client.clone(), self.graph.clone(), commit.to_owned());
                let read = self.start(
                    "node_history_blob",
                    async move {
                        client
                            .blob_head(&graph, &cell, BlobAt::Snapshot(&commit))
                            .await
                    },
                    cx,
                );
                cx.background_executor()
                    .spawn(async move { Ok(Answer::Blob(ix, BlobState::of(read.await?))) })
            }
        }
    }

    /// Runs `jobs` (a commit's index into `ids` and a read), [`NODE_HISTORY_READS_AT_ONCE`] at
    /// a time, and writes each answer into `states`. Returns the failures, by commit index, and
    /// adds to `full` each group a read found full.
    async fn run(
        &mut self,
        ids: &[String],
        jobs: Vec<(usize, Read)>,
        states: &mut [NodeState],
        full: &mut Vec<usize>,
        cx: &mut AsyncApp,
    ) -> Vec<(usize, ReadError)> {
        let mut failed = Vec::new();
        for chunk in jobs.chunks(NODE_HISTORY_READS_AT_ONCE) {
            let tasks: Vec<_> = chunk
                .iter()
                .map(|&(at, read)| (at, self.read_at(read, &ids[at], cx)))
                .collect();
            for (at, task) in tasks {
                match task.await {
                    Ok(Answer::Node(node)) => {
                        let state = &mut states[at];
                        state.exists = node.is_some();
                        state.properties = node.unwrap_or_default();
                    }
                    Ok(Answer::Edges(ix, edges, was_full)) => {
                        if was_full {
                            states[at].full.insert(ix);
                            if !full.contains(&ix) {
                                full.push(ix);
                            }
                        }
                        states[at].edges.insert(ix, edges);
                    }
                    Ok(Answer::Blob(ix, blob)) => {
                        let name = self.node.blobs[ix].clone();
                        states[at].blobs.insert(name, blob);
                    }
                    Err(error) => failed.push((at, error)),
                }
            }
        }
        failed
    }

    /// The node at `commit`, read once.
    async fn node_at(&mut self, commit: &str, cx: &mut AsyncApp) -> Result<NodeState, ReadError> {
        let mut states = vec![NodeState::default()];
        let failed = self
            .run(
                &[commit.to_owned()],
                vec![(0, Read::Node)],
                &mut states,
                &mut Vec::new(),
                cx,
            )
            .await;
        match failed.into_iter().next() {
            Some((_, error)) => Err(error),
            None => Ok(states.remove(0)),
        }
    }
}

/// The schema change a refused read at a commit names, when that is why it was refused: the
/// type the gap is in.
fn gap_of(error: &ReadError) -> Option<Option<String>> {
    let ReadError::Refused(refusal) = error else {
        return None;
    };
    Some(match refusal.historical_gap()? {
        HistoricalGap::Type { name } => Some(name),
        HistoricalGap::Property { owner, .. } => Some(owner),
    })
}

impl GraphSession {
    fn history_reader(
        &self,
        scope: &ReadScope,
        branch: &str,
        node: NodeScope,
        cx: &mut Context<Self>,
    ) -> Option<Reader> {
        Some(Reader {
            session: cx.entity().downgrade(),
            asker: self.asker_of(scope),
            client: self.client(cx),
            graph: self.graph_id.clone(),
            branch: branch.to_owned(),
            schema: self.schema_model()?,
            node,
            requests: 0,
        })
    }

    /// A window of node `node`'s history on `branch` (spec 046), asked for through `scope`: the
    /// newest [`NODE_HISTORY_COMMITS`] first-parent commits, or the ones before a window read
    /// already. One commit listing, then at each commit and at the oldest one's parent the node
    /// itself, its edges through each relationship group and its Blobs' sizes, at most
    /// [`NODE_HISTORY_READS_AT_ONCE`] at once; the edges and Blobs only where the node exists.
    /// Then the branch of each merge that changed the node. Each read
    /// is named in the ledger; nothing is asked again by itself. The newest window keeps the
    /// node's last change for its page ([`Self::last_change`]).
    pub fn node_history_in(
        &mut self,
        scope: &ReadScope,
        branch: &str,
        node: NodeScope,
        window: NodeHistoryWindow,
        cx: &mut Context<Self>,
    ) -> Task<Result<NodeHistoryPage, ReadError>> {
        let Some(mut reader) = self.history_reader(scope, branch, node, cx) else {
            return Task::ready(Err(ReadError::Decode("the schema is not read yet".into())));
        };
        cx.spawn(async move |_, cx| {
            let newest = matches!(window, NodeHistoryWindow::Newest);
            let (token, head, carried) = match window {
                NodeHistoryWindow::Newest => (None, None, None),
                NodeHistoryWindow::Older { head, window } => {
                    (Some(window.page_token), Some(head), window.base)
                }
            };
            let (client, graph, branch) = (
                reader.client.clone(),
                reader.graph.clone(),
                reader.branch.clone(),
            );
            let listing = reader
                .start(
                    "node_history_commits",
                    async move {
                        client
                            .commit_page(&graph, &branch, NODE_HISTORY_COMMITS, token.as_deref())
                            .await
                    },
                    cx,
                )
                .await?;
            let head = head
                .or(listing.head_commit_id.clone())
                .or_else(|| listing.commits.first().map(|c| c.graph_commit_id.clone()))
                .unwrap_or_default();
            let mut commits: Vec<WindowCommit> =
                listing.commits.iter().map(WindowCommit::of).collect();
            let mut page = NodeHistoryPage {
                head,
                newest: None,
                oldest: None,
                entries: Vec::new(),
                commits: Vec::new(),
                older: None,
                stopped: None,
                boundary: None,
                full_groups: Vec::new(),
                requests: 0,
            };
            let Some(oldest) = commits.last().cloned() else {
                page.requests = reader.requests;
                return Ok(page);
            };
            // The commits read: the window's, then the oldest one's parent, which the window's
            // oldest change is measured against.
            let mut ids: Vec<String> = commits.iter().map(|c| c.id.clone()).collect();
            ids.extend(oldest.parent.clone());
            let mut states = vec![NodeState::default(); ids.len()];
            let carried = carried.filter(|(id, _)| *id == ids[0]);
            let first = usize::from(carried.is_some());
            if let Some((_, state)) = carried {
                states[0] = state;
            }
            let last = ids.len() - 1;
            let groups = reader.node.groups.len();
            let blobs = reader.node.blobs.len();
            let mut full = Vec::new();

            // The node at every commit; then, at each commit that holds it, its edges through
            // every relationship group and its Blobs. No group is left out: a group read only at
            // the window's ends would miss an edge added and removed between them, and with it
            // who changed the node last.
            let jobs: Vec<(usize, Read)> = (first..ids.len()).map(|at| (at, Read::Node)).collect();
            let mut failed = reader.run(&ids, jobs, &mut states, &mut full, cx).await;
            let mut jobs = Vec::new();
            for (at, _) in states
                .iter()
                .enumerate()
                .skip(first)
                .filter(|(_, s)| s.exists)
            {
                jobs.extend((0..groups).map(|ix| (at, Read::Edges(ix))));
                jobs.extend((0..blobs).map(|ix| (at, Read::Blob(ix))));
            }
            failed.extend(reader.run(&ids, jobs, &mut states, &mut full, cx).await);
            page.full_groups = full
                .iter()
                .map(|&ix| reader.node.groups[ix].clone())
                .collect();

            // A failed read ends what is known at the newest commit it failed at: a schema change
            // between it and the commit above it (E52), or a read that stopped short.
            failed.sort_by_key(|(at, _)| *at);
            if let Some((at, error)) = failed.into_iter().next() {
                if at == 0 {
                    return Err(error);
                }
                match gap_of(&error) {
                    Some(type_name) => {
                        page.boundary = Some(SchemaBoundary {
                            commit: ids[at - 1].clone(),
                            type_name,
                        });
                    }
                    None => page.stopped = Some(error.to_string()),
                }
                // Commit `at - 1` is measured against `at`, which was not read.
                commits.truncate(at - 1);
                states.truncate(at);
            } else {
                page.older = listing
                    .next_page_token
                    .clone()
                    .map(|page_token| OlderWindow {
                        page_token,
                        base: oldest.parent.clone().map(|id| (id, states[last].clone())),
                    });
                page.oldest = Some((ids[last].clone(), states[last].clone()));
            }
            page.entries = entries_of(&reader.node, &commits, &states);
            page.newest = states.first().cloned();

            // The branch each merge came from, a commit read each, the newest first.
            let merges: Vec<(usize, String)> = page
                .entries
                .iter()
                .enumerate()
                .filter_map(|(ix, e)| e.merged_parent.clone().map(|m| (ix, m)))
                .take(NODE_HISTORY_MERGE_NAMES)
                .collect();
            for (ix, merged) in merges {
                let (client, graph) = (reader.client.clone(), reader.graph.clone());
                let read = reader
                    .start(
                        "node_history_merge",
                        async move { client.commit(&graph, &merged).await },
                        cx,
                    )
                    .await;
                if let Ok(commit) = read {
                    page.entries[ix].merged_branch =
                        Some(commit.graph_branch.unwrap_or_else(|| "main".to_owned()));
                }
            }

            page.commits = commits;
            page.requests = reader.requests;
            // A group too large to follow may hide a newer change: the page then says nothing.
            if newest
                && page.full_groups.is_empty()
                && let Some(last) = page.entries.first()
            {
                let last = LastChange {
                    head: page.head.clone(),
                    commit: last.commit.clone(),
                    actor: last.actor.clone(),
                    at: last.at,
                };
                let (type_name, id) = (reader.node.type_name.clone(), reader.node.id.clone());
                reader
                    .session
                    .update(cx, |session, cx| {
                        session.note_last_change(&type_name, &id, last, cx)
                    })
                    .ok();
            }
            Ok(page)
        })
    }

    /// Searches past the windows read (spec 046), asked for through `scope`: when node `node`
    /// was created, if it existed at `base` (the oldest commit read, with the node there), and
    /// which commit set each of `values` (properties no commit read changed, with their value at
    /// `base`). It lists the commits from `base` back, from `page_token` for the head the windows
    /// answer for, in pages of 1,000, then binary-searches them with point reads of the node,
    /// shared by every question. Only for these: a list of changes is never searched, since a
    /// search misses a value that changed and changed back.
    #[allow(clippy::too_many_arguments)]
    pub fn node_history_search(
        &mut self,
        scope: &ReadScope,
        branch: &str,
        node: NodeScope,
        page_token: String,
        base: (String, NodeState),
        values: Vec<(String, Value)>,
        cx: &mut Context<Self>,
    ) -> Task<Result<SearchFound, ReadError>> {
        let Some(mut reader) = self.history_reader(scope, branch, node, cx) else {
            return Task::ready(Err(ReadError::Decode("the schema is not read yet".into())));
        };
        cx.spawn(async move |_, cx| {
            let mut found = SearchFound::default();
            // The commits from `base` back towards the graph's first.
            let mut older: Vec<WindowCommit> = Vec::new();
            let mut token = Some(page_token);
            while let Some(at) = token.take() {
                if older.len() >= NODE_HISTORY_SEARCH_COMMITS {
                    found.stopped = Some(format!(
                        "Studio searched the {NODE_HISTORY_SEARCH_COMMITS} commits before the \
                         windows read"
                    ));
                    break;
                }
                let (client, graph, branch) = (
                    reader.client.clone(),
                    reader.graph.clone(),
                    reader.branch.clone(),
                );
                let listing = reader
                    .start(
                        "node_history_search_commits",
                        async move { client.commit_page(&graph, &branch, 1000, Some(&at)).await },
                        cx,
                    )
                    .await?;
                older.extend(listing.commits.iter().map(WindowCommit::of));
                token = listing.next_page_token;
            }
            let reached_first = token.is_none() && found.stopped.is_none();
            if older.first().map(|c| c.id.as_str()) != Some(base.0.as_str()) {
                return Err(ReadError::Decode(
                    "the commits before the windows do not go on from where they ended".into(),
                ));
            }
            let mut known: HashMap<usize, NodeState> = HashMap::from([(0, base.1.clone())]);
            let questions: Vec<Option<(String, Value)>> = base
                .1
                .exists
                .then_some(None)
                .into_iter()
                .chain(values.into_iter().map(Some))
                .collect();
            for question in questions {
                // Binary search for the oldest commit the answer still holds at.
                let (mut lo, mut hi) = (0usize, older.len());
                let mut failed = None;
                while let Some(mid) = search_step(lo, hi) {
                    let state = match known.get(&mid) {
                        Some(state) => state.clone(),
                        None => match reader.node_at(&older[mid].id, cx).await {
                            Ok(state) => {
                                known.insert(mid, state.clone());
                                state
                            }
                            Err(error) => {
                                failed = Some(error);
                                break;
                            }
                        },
                    };
                    let holds = state.exists
                        && match &question {
                            None => true,
                            Some((name, value)) => {
                                state.properties.get(name).unwrap_or(&Value::Null) == value
                            }
                        };
                    if holds {
                        lo = mid;
                    } else {
                        hi = mid;
                    }
                }
                if let Some(error) = failed {
                    found.stopped = Some(error.to_string());
                    break;
                }
                match (question, settled(lo, older.len(), reached_first)) {
                    (None, Some(at)) => found.created = Some(older[at].clone()),
                    (Some((name, _)), Some(at)) => {
                        found.set.insert(name, older[at].clone());
                    }
                    (_, None) => found.before = older.last().cloned(),
                }
            }
            found.requests = reader.requests;
            Ok(found)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SCHEMA: &str = "node Person {\n    slug: String @key\n    name: String?\n    \
        title: String?\n    email: String?\n}\n\
        node Company {\n    slug: String @key\n}\n\
        edge Knows: Person -> Person\n\
        edge WorksAt: Person -> Company\n";

    fn scope() -> NodeScope {
        NodeScope::new(&SchemaModel::parse(SCHEMA).unwrap(), "Person", "ada")
    }

    fn commit(n: usize, actor: &str) -> WindowCommit {
        WindowCommit {
            id: format!("c{n}"),
            parent: n.checked_sub(1).map(|p| format!("c{p}")),
            at: n as i64 * 60 * 1_000_000,
            actor: Some(actor.into()),
            branch: "main".into(),
            merged_parent: None,
        }
    }

    fn state(props: Value, edges: &[(usize, &str, &str)]) -> NodeState {
        let mut state = NodeState {
            exists: true,
            properties: props.as_object().unwrap().clone(),
            ..Default::default()
        };
        for (ix, id, other) in edges {
            state
                .edges
                .entry(*ix)
                .or_default()
                .insert((*id).into(), (*other).into());
        }
        state
    }

    /// The groups are read at both ends of the node's type, and a commit costs one read for the
    /// node and one for each group.
    #[test]
    fn the_scope_reads_each_relationship_group_at_each_end() {
        let scope = scope();
        let groups: Vec<(&str, Direction)> = scope
            .groups
            .iter()
            .map(|g| (g.edge.as_str(), g.direction))
            .collect();
        assert_eq!(
            groups,
            [
                ("Knows", Direction::Outgoing),
                ("WorksAt", Direction::Outgoing),
                ("Knows", Direction::Incoming),
            ]
        );
        assert_eq!(scope.reads_per_commit(), 4);
    }

    /// Consecutive reads that differ are the newer commit's change: a property changed, a value
    /// changed and changed back (two entries, where a net diff sees none), edges added at either
    /// end, the node created and removed.
    #[test]
    fn consecutive_reads_make_the_entries() {
        let scope = scope();
        let commits: Vec<WindowCommit> = (1..=5).rev().map(|n| commit(n, "act-a")).collect();
        let linked = [(2, "k1", "grace"), (1, "w1", "acme")];
        let states = vec![
            NodeState::default(),
            state(json!({"name": "Ada", "title": "A"}), &linked),
            state(json!({"name": "Ada", "title": "B"}), &linked),
            state(json!({"name": "Ada", "title": "A"}), &linked),
            state(json!({"name": "Ada", "title": "A"}), &[]),
            NodeState::default(),
        ];
        let entries = entries_of(&scope, &commits, &states);
        let shown: Vec<(&str, bool, bool, usize)> = entries
            .iter()
            .map(|e| (e.commit.as_str(), e.created(), e.removed(), e.edges.len()))
            .collect();
        assert_eq!(
            shown,
            [
                ("c5", false, true, 2),
                ("c4", false, false, 0),
                ("c3", false, false, 0),
                ("c2", false, false, 2),
                ("c1", true, false, 0),
            ]
        );
        let ends = |name: &str| {
            entries[3]
                .edges
                .iter()
                .find(|e| e.type_name == name)
                .and_then(|e| e.endpoints())
        };
        assert_eq!(
            ends("Knows"),
            Some(("grace".into(), "ada".into())),
            "incoming"
        );
        assert_eq!(
            ends("WorksAt"),
            Some(("ada".into(), "acme".into())),
            "outgoing"
        );
    }

    /// "Now": each property's newest setter within what was read, what a search found, and
    /// "before" the bound otherwise.
    #[test]
    fn blame_names_the_newest_setter_and_says_before_past_the_window() {
        let scope = scope();
        let commits: Vec<WindowCommit> = (2..=3).rev().map(|n| commit(n, "act-a")).collect();
        let states = vec![
            state(json!({"name": "Ada", "title": "B", "email": "b@x"}), &[]),
            state(json!({"name": "Ada", "title": "B", "email": "a@x"}), &[]),
            state(json!({"name": "Ada", "title": "A", "email": "a@x"}), &[]),
        ];
        let entries = entries_of(&scope, &commits, &states);
        let props: Vec<String> = ["name", "title", "email"].map(String::from).into();
        let bound = commit(1, "act-a");
        let mut found = HashMap::new();
        let now = blame(&entries, &props, states.first(), &found, Some(&bound));
        assert_eq!(now[0].set, BlameSet::Before(bound.clone()));
        assert_eq!(now[0].value, Some(json!("Ada")));
        assert_eq!(
            now[1].set,
            BlameSet::In {
                entry: 1,
                created: false
            }
        );
        assert_eq!(
            now[2].set,
            BlameSet::In {
                entry: 0,
                created: false
            }
        );
        found.insert("name".to_owned(), commit(0, "act-old"));
        let now = blame(&entries, &props, states.first(), &found, Some(&bound));
        assert_eq!(now[0].set, BlameSet::Found(commit(0, "act-old")));
    }

    fn entry(
        commit: &str,
        actor: &str,
        minutes: i64,
        node: Option<(&str, &str)>,
    ) -> NodeHistoryEntry {
        let image = |title: &str| ChangeImageOutput {
            properties: json!({"title": title}).as_object().unwrap().clone(),
            endpoints: None,
        };
        NodeHistoryEntry {
            commit: commit.into(),
            actor: Some(actor.into()),
            at: minutes * 60 * 1_000_000,
            branch: "main".into(),
            merged_parent: None,
            merged_branch: None,
            node: node.map(|(before, after)| FoldedChange {
                kind: EntityKind::Node,
                type_id: "Person".into(),
                type_name: "Person".into(),
                id: "ada".into(),
                before: Some(image(before)),
                after: Some(image(after)),
                commits: vec![commit.into()],
            }),
            edges: Vec::new(),
        }
    }

    /// A run of one actor's edits, each within ten minutes of the next, folds into one with its
    /// net change; another actor, a gap of ten minutes or a merge ends it.
    #[test]
    fn a_run_of_small_edits_folds_with_its_net_change() {
        let mut merge = entry("c1", "act-a", 15, Some(("Z", "A")));
        merge.merged_parent = Some("b1".into());
        let entries = vec![
            entry("c6", "act-a", 40, Some(("C", "D"))),
            entry("c5", "act-a", 35, Some(("B", "C"))),
            entry("c4", "act-a", 31, Some(("A", "B"))),
            entry("c3", "act-b", 30, Some(("A", "A2"))),
            entry("c2", "act-b", 18, Some(("A2", "A"))),
            merge,
        ];
        let folded = runs(&entries, &[false; 6]);
        assert_eq!(folded, [0..3, 3..4, 4..5, 5..6]);
        let (node, edges) = net_of(&entries[folded[0].clone()]);
        let node = node.unwrap();
        assert_eq!(node.before.unwrap().properties["title"], json!("A"));
        assert_eq!(node.after.unwrap().properties["title"], json!("D"));
        assert_eq!(node.commits, ["c4", "c5", "c6"]);
        assert!(edges.is_empty());
        let studio = [true, false, false, false, false, false];
        assert_eq!(runs(&entries, &studio)[0], 0..1);
    }

    /// The binary search finds the oldest commit a question holds at in log2 steps.
    #[test]
    fn the_search_finds_the_oldest_commit_that_holds() {
        let (mut lo, mut hi, mut asked) = (0, 464, 0);
        while let Some(mid) = search_step(lo, hi) {
            asked += 1;
            if mid <= 300 {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        assert_eq!(lo, 300);
        assert!(asked <= 9, "{asked} probes");
    }

    /// A group read full at either of two commits is not diffed between them: its page holds the
    /// first 1,000 edges by id, so an edge added before them pushes the last one off the page,
    /// which is no removal.
    #[test]
    fn a_full_group_is_not_diffed() {
        let scope = scope();
        let commits = vec![commit(2, "act-a")];
        let mut older = state(json!({"name": "Ada"}), &[(0, "e2", "x"), (0, "e3", "y")]);
        let mut newer = state(json!({"name": "Ada"}), &[(0, "e1", "z"), (0, "e2", "x")]);
        older.full.insert(0);
        newer.full.insert(0);
        assert!(entries_of(&scope, &commits, &[newer.clone(), older.clone()]).is_empty());
        newer.full.clear();
        older.full.clear();
        assert_eq!(
            entries_of(&scope, &commits, &[newer, older])[0].edges.len(),
            2
        );
    }

    /// A search that listed commits up to its cap, its answer still holding at the oldest it
    /// listed, names no commit: the value was set before it, not by it.
    #[test]
    fn a_capped_search_names_no_commit_at_its_end() {
        assert_eq!(settled(3, 10, false), Some(3));
        assert_eq!(settled(9, 10, true), Some(9), "the graph's first commit");
        assert_eq!(
            settled(9, 10, false),
            None,
            "the listing stopped at its cap"
        );
    }

    /// Blob cells compare by kind and size, said as a size.
    #[test]
    fn a_blob_is_compared_by_kind_and_size() {
        assert_eq!(BlobState::Stored { size: 1_200 }.value(), json!("1.2 KB"));
        assert_ne!(
            BlobState::Stored { size: 1 },
            BlobState::External {
                uri: "s3://x".into()
            }
        );
        assert_eq!(BlobState::Null.value(), Value::Null);
    }
}
