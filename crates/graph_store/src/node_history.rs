//! One node's history (spec 046): the commits of a branch's recent first-parent history that
//! changed the node or one of its relationships, newest first, each with who made it and how,
//! and runs of one actor's small edits folded into one.
//!
//! Omnigraph has no read of the commits that changed one entity (E62). Studio reads the node
//! itself instead: the commit listing names a window of the branch's newest
//! [`NODE_HISTORY_FIRST_COMMITS`] first-parent commits (then [`NODE_HISTORY_COMMITS`] a window
//! further back), and at each of them, and at the parent of the
//! oldest, Studio reads the node with a point query at that commit's snapshot, its edges through
//! each relationship group (their ids, and a keyed edge's properties outside its key, which an
//! upsert changes under the same id), and the size and kind of each Blob. Two consecutive reads
//! that differ are the change the newer commit made. The cost follows the window and the
//! node's relationship groups, never what the node's type wrote: a load of 50,000 nodes of the
//! type in the window costs what any other commit costs. It is exact for properties, a value
//! changed and changed back included. An older window goes on from the listing's page token, for
//! the head the first window resolved (constitution II).
//!
//! What a window followed and what it could not ([`Coverage`]) comes with it, so the view says
//! nothing the reads do not support. Entries and coverage do not depend on how they were read:
//! E62 can replace the read plan alone.
//!
//! The change feed (`GET /changes` with `type` and `op`) is the alternative where a type changes
//! rarely; it is not used here (spec 046, Decisions).

use std::collections::{BTreeMap, HashMap};
use std::ops::Range;

use futures::StreamExt as _;
use futures::stream::FuturesUnordered;
use gpui::{AsyncApp, Context, Task, WeakEntity};
use gq_language::{BuiltQuery, Direction, RelationshipGroup, ScalarKind, SchemaModel, builder};
use graph_client::api::{ChangeImageOutput, EntityKind, QueryRequest};
use graph_client::{BlobAt, BlobCell, BlobMeta, GraphClient, HistoricalGap, ReadError};
use serde_json::{Map, Value};

use crate::GraphSession;
use crate::changes::{FoldedChange, NetChange};
use crate::ledger::Asker;
use crate::scope::ReadScope;

/// First-parent commits an older window of a node's history covers ("Read the 200 commits
/// before").
pub const NODE_HISTORY_COMMITS: u32 = 200;

/// First-parent commits the first window covers: smaller, so History opens in a few seconds. A
/// full window of 200 on a person with four relationship groups is about 1,000 requests, which
/// settled in 18 s against a local server (spec 046, validation log).
pub const NODE_HISTORY_FIRST_COMMITS: u32 = 50;

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

    /// The cell as an image holds it, exact: `{"size": n}` for a file Omnigraph stores,
    /// `{"uri": …}` for an external one. Images are compared as they are, so two files that
    /// differ by a byte are two values; a card formats the size (`change_cards`).
    pub fn image(&self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Stored { size } => serde_json::json!({ "size": size }),
            Self::External { uri } => serde_json::json!({ "uri": uri }),
        }
    }
}

/// One edge of the node as a commit holds it: its other end, and for a keyed edge its
/// properties outside the key, which an upsert changes in place.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EdgeAt {
    pub other: String,
    pub values: Map<String, Value>,
}

/// The node as one commit holds it: whether it exists, its properties, its Blobs and its edges
/// through each relationship group (by the group's index), each edge by its id.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NodeState {
    pub exists: bool,
    pub properties: Map<String, Value>,
    pub blobs: BTreeMap<String, BlobState>,
    pub edges: BTreeMap<usize, BTreeMap<String, EdgeAt>>,
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
    /// For each group, the properties a keyed edge of its type changes in place, read with its
    /// ids; none for an edge without a key.
    in_place: Vec<Vec<String>>,
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
        let groups = schema.relationships_of(type_name).to_vec();
        let in_place = groups
            .iter()
            .map(|group| {
                schema
                    .edge(&group.edge)
                    .map(|edge| {
                        edge.changed_in_place()
                            .iter()
                            .map(|p| p.name.clone())
                            .collect()
                    })
                    .unwrap_or_default()
            })
            .collect();
        Self {
            type_name: type_name.to_owned(),
            id: id.to_owned(),
            groups,
            in_place,
            blobs,
        }
    }

    /// Reads at one commit: the node, one per relationship group, one per Blob property.
    pub fn reads_per_commit(&self) -> usize {
        1 + self.groups.len() + self.blobs.len()
    }

    /// What changed from `older` to `newer` in `commit`: the node itself, and each edge added,
    /// removed, or for a keyed edge changed in place.
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
                    properties.insert(name.clone(), blob.image());
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
            let image = |edge: &EdgeAt| {
                let other = edge.other.as_str();
                let (from, to) = match group.direction {
                    Direction::Outgoing => (self.id.as_str(), other),
                    Direction::Incoming => (other, self.id.as_str()),
                };
                ChangeImageOutput {
                    properties: edge.values.clone(),
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
            for (id, at) in now {
                match was.get(id) {
                    None => edges.push(edge(id, None, Some(image(at)))),
                    // A keyed edge upserted under the same id: its properties changed in place.
                    Some(before) if before != at => {
                        edges.push(edge(id, Some(image(before)), Some(image(at))))
                    }
                    Some(_) => {}
                }
            }
            for (id, at) in was.iter().filter(|(id, _)| !now.contains_key(*id)) {
                edges.push(edge(id, Some(image(at)), None));
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

/// How far back the windows of a node's history reach.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Reach {
    /// Older commits remain unread.
    #[default]
    Window,
    /// The graph's first commit.
    GraphStart,
    /// A schema change a read could not cross (E52).
    Boundary(SchemaBoundary),
    /// A read failed, for this reason: what is older is not known.
    Stopped(String),
}

/// What is known of the node's creation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Creation {
    /// Not searched, and no entry read created it.
    #[default]
    NotSearched,
    /// An entry read created it.
    InEntries,
    /// A search of the commits before the windows found the commit that created it.
    Found(WindowCommit),
    /// The search listed commits up to its cap with the node still there: it was created
    /// before this commit, the oldest listed.
    Before(WindowCommit),
    /// The search stopped short, for this reason.
    SearchStopped(String),
}

/// What a node's history followed, and what it could not (spec 046): every sentence that says
/// what was read, or that nothing changed, reads it from here, so none claims more than the
/// reads support.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Coverage {
    /// Relationship groups too large to follow at some commit read ([`NODE_HISTORY_EDGES`] edges
    /// or more): their changes there are not known. Every other group was followed.
    pub too_large: Vec<RelationshipGroup>,
    pub reach: Reach,
    pub creation: Creation,
}

impl Coverage {
    /// Whether every relationship group was followed at every commit read.
    pub fn follows_every_group(&self) -> bool {
        self.too_large.is_empty()
    }

    /// Whether "no commit changed it since the graph began" can be said: the windows reach the
    /// graph's first commit and followed every group.
    pub fn reaches_the_start(&self) -> bool {
        self.reach == Reach::GraphStart && self.follows_every_group()
    }

    /// Whether the newest entry is the node's last change: no group too large to follow may
    /// hide a newer one.
    pub fn last_change_known(&self) -> bool {
        self.follows_every_group()
    }

    /// Takes an older window's coverage below this one's: the groups it could not follow, how
    /// far it reaches, and a creation among its entries.
    pub fn below(&mut self, older: Coverage) {
        for group in older.too_large {
            if !self.too_large.contains(&group) {
                self.too_large.push(group);
            }
        }
        self.reach = older.reach;
        if older.creation == Creation::InEntries {
            self.creation = Creation::InEntries;
        }
    }

    /// Takes what a search past the windows found of the node's creation.
    pub fn searched(&mut self, found: &SearchFound) {
        if self.creation != Creation::InEntries {
            self.creation = found.creation.clone();
        }
    }
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
    /// What the window followed and what it could not.
    pub coverage: Coverage,
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

/// What the search past the windows found of the node's creation.
#[derive(Debug, Clone, Default)]
pub struct SearchFound {
    pub creation: Creation,
    /// Requests sent: listing pages and point reads.
    pub requests: usize,
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
    Edges(usize, BTreeMap<String, EdgeAt>, bool),
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
                let in_place = self.node.in_place[ix].clone();
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
                            let values = in_place
                                .iter()
                                .map(|name| {
                                    let value = row.get(format!("v_{name}")).cloned();
                                    (name.clone(), value.unwrap_or(Value::Null))
                                })
                                .collect();
                            Some((
                                row.get("edge")?.as_str()?.to_owned(),
                                EdgeAt {
                                    other: row.get("id")?.as_str()?.to_owned(),
                                    values,
                                },
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

    /// Runs `jobs` (a commit's index into `ids` and a read), keeping
    /// [`NODE_HISTORY_READS_AT_ONCE`] running, a new one starting as each answers, and writes
    /// each answer into `states`. Returns the failures, by commit index, and
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
        let mut jobs = jobs.into_iter();
        let mut running = FuturesUnordered::new();
        loop {
            while running.len() < NODE_HISTORY_READS_AT_ONCE {
                let Some((at, read)) = jobs.next() else {
                    break;
                };
                let task = self.read_at(read, &ids[at], cx);
                running.push(async move { (at, task.await) });
            }
            let Some((at, answer)) = running.next().await else {
                break;
            };
            {
                match answer {
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
    /// newest [`NODE_HISTORY_FIRST_COMMITS`] first-parent commits, or the
    /// [`NODE_HISTORY_COMMITS`] before a window read already. One commit listing, then at each commit and at the oldest one's parent the node
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
            let limit = if newest {
                NODE_HISTORY_FIRST_COMMITS
            } else {
                NODE_HISTORY_COMMITS
            };
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
                            .commit_page(&graph, &branch, limit, token.as_deref())
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
                coverage: Coverage::default(),
                requests: 0,
            };
            let Some(oldest) = commits.last().cloned() else {
                page.coverage.reach = Reach::GraphStart;
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
            full.sort_unstable();
            page.coverage.too_large = full
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
                page.coverage.reach = match gap_of(&error) {
                    Some(type_name) => Reach::Boundary(SchemaBoundary {
                        commit: ids[at - 1].clone(),
                        type_name,
                    }),
                    None => Reach::Stopped(error.to_string()),
                };
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
                if page.older.is_none() {
                    page.coverage.reach = Reach::GraphStart;
                }
            }
            page.entries = entries_of(&reader.node, &commits, &states);
            if page.entries.iter().any(NodeHistoryEntry::created) {
                page.coverage.creation = Creation::InEntries;
            }
            page.newest = states.first().cloned();

            // The branch each merge came from, a commit read each, the newest first.
            let merges: Vec<(usize, String)> = page
                .entries
                .iter()
                .enumerate()
                .filter_map(|(ix, e)| e.merged_parent.clone().map(|m| (ix, m)))
                .take(NODE_HISTORY_MERGE_NAMES)
                .collect();
            // Read as the window's reads are, at most NODE_HISTORY_READS_AT_ONCE at once.
            let mut merges = merges.into_iter();
            let mut running = FuturesUnordered::new();
            loop {
                while running.len() < NODE_HISTORY_READS_AT_ONCE {
                    let Some((ix, merged)) = merges.next() else {
                        break;
                    };
                    let (client, graph) = (reader.client.clone(), reader.graph.clone());
                    let read = reader.start(
                        "node_history_merge",
                        async move { client.commit(&graph, &merged).await },
                        cx,
                    );
                    running.push(async move { (ix, read.await) });
                }
                let Some((ix, read)) = running.next().await else {
                    break;
                };
                if let Ok(commit) = read {
                    page.entries[ix].merged_branch =
                        Some(commit.graph_branch.unwrap_or_else(|| "main".to_owned()));
                }
            }

            page.commits = commits;
            page.requests = reader.requests;
            // A group too large to follow may hide a newer change: the page then says nothing.
            if newest
                && page.coverage.last_change_known()
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
    /// was created, it being there at `base` (the oldest commit read). It lists the commits from
    /// `base` back, from `page_token` for the head the windows answer for, in pages of 1,000,
    /// then binary-searches them with point reads of the node. Only for its creation: a list of
    /// changes is never searched, since a search misses a value that changed and changed back.
    pub fn node_history_search(
        &mut self,
        scope: &ReadScope,
        branch: &str,
        node: NodeScope,
        page_token: String,
        base: (String, NodeState),
        cx: &mut Context<Self>,
    ) -> Task<Result<SearchFound, ReadError>> {
        let Some(mut reader) = self.history_reader(scope, branch, node, cx) else {
            return Task::ready(Err(ReadError::Decode("the schema is not read yet".into())));
        };
        if !base.1.exists {
            return Task::ready(Err(ReadError::Decode(
                "the node is not there at the oldest commit read".into(),
            )));
        }
        cx.spawn(async move |_, cx| {
            let mut found = SearchFound::default();
            let mut capped = false;
            // The commits from `base` back towards the graph's first.
            let mut older: Vec<WindowCommit> = Vec::new();
            let mut token = Some(page_token);
            while let Some(at) = token.take() {
                if older.len() >= NODE_HISTORY_SEARCH_COMMITS {
                    capped = true;
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
            let reached_first = token.is_none() && !capped;
            if older.first().map(|c| c.id.as_str()) != Some(base.0.as_str()) {
                return Err(ReadError::Decode(
                    "the commits before the windows do not go on from where they ended".into(),
                ));
            }
            // Binary search for the oldest commit the node is still there at.
            let (mut lo, mut hi) = (0usize, older.len());
            while let Some(mid) = search_step(lo, hi) {
                match reader.node_at(&older[mid].id, cx).await {
                    Ok(state) if state.exists => lo = mid,
                    Ok(_) => hi = mid,
                    Err(error) => {
                        found.creation = Creation::SearchStopped(error.to_string());
                        found.requests = reader.requests;
                        return Ok(found);
                    }
                }
            }
            found.creation = match (settled(lo, older.len(), reached_first), older.last()) {
                (Some(at), _) => Creation::Found(older[at].clone()),
                (None, Some(oldest)) => Creation::Before(oldest.clone()),
                (None, None) => Creation::NotSearched,
            };
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
            state.edges.entry(*ix).or_default().insert(
                (*id).into(),
                EdgeAt {
                    other: (*other).into(),
                    values: Map::new(),
                },
            );
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
    /// listed, names no commit: the node was created before it, not by it.
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

    /// Blob cells compare exactly, by kind and size: a file of 1,200 bytes replaced by one of
    /// 1,240 is a change, where their rounded sizes ("1.2 KB") read alike (review of #162).
    #[test]
    fn a_blob_is_compared_by_its_exact_size() {
        let scope = NodeScope::new(
            &SchemaModel::parse("node Doc {\n  slug: String @key\n  file: Blob?\n}\n").unwrap(),
            "Doc",
            "d1",
        );
        let with = |size: u64| {
            let mut state = state(json!({"slug": "d1"}), &[]);
            state
                .blobs
                .insert("file".into(), BlobState::Stored { size });
            state
        };
        let (older, newer) = (with(1_200), with(1_240));
        assert_ne!(older, newer);
        let entries = entries_of(&scope, &[commit(2, "act-a")], &[newer, older]);
        assert_eq!(entries.len(), 1, "one entry");
        assert_eq!(
            BlobState::Stored { size: 1_240 }.image(),
            json!({"size": 1_240})
        );
        assert_eq!(BlobState::Null.image(), Value::Null);
        assert_ne!(
            BlobState::Stored { size: 1 },
            BlobState::External {
                uri: "s3://x".into()
            }
        );
    }

    /// A keyed edge upserted under the same id with a new property value is an entry: its id
    /// alone would miss it (review of #162).
    #[test]
    fn a_keyed_edge_changed_in_place_is_an_entry() {
        let schema = SchemaModel::parse(
            "node Person {\n  slug: String @key\n}\n\
             edge Knows: Person -> Person {\n  since: String?\n  @key(@src, @dst)\n}\n",
        )
        .unwrap();
        let scope = NodeScope::new(&schema, "Person", "ada");
        assert!(scope.in_place.iter().any(|names| names == &["since"]));
        let ix = scope
            .groups
            .iter()
            .position(|g| g.direction == Direction::Outgoing)
            .unwrap();
        let knows = |since: &str| {
            let mut state = state(json!({"slug": "ada"}), &[]);
            let mut values = Map::new();
            values.insert("since".into(), json!(since));
            state.edges.entry(ix).or_default().insert(
                "k1".into(),
                EdgeAt {
                    other: "grace".into(),
                    values,
                },
            );
            state
        };
        let entries = entries_of(
            &scope,
            &[commit(2, "act-a")],
            &[knows("2025"), knows("2024")],
        );
        assert_eq!(entries.len(), 1);
        let edge = &entries[0].edges[0];
        assert!(
            edge.before.is_some() && edge.after.is_some(),
            "changed, not added"
        );
        assert!(
            entries_of(
                &scope,
                &[commit(2, "act-a")],
                &[knows("2024"), knows("2024")]
            )
            .is_empty()
        );
    }

    /// Coverage says what can be claimed: "since the graph began" only when the windows reach
    /// it and every group was followed; the last change only when no group was too large; an
    /// older window's groups and reach join it, and a search's creation, unless an entry holds
    /// it (review of #162).
    #[test]
    fn coverage_says_only_what_the_reads_support() {
        let group = scope().groups[0].clone();
        let mut coverage = Coverage {
            reach: Reach::GraphStart,
            ..Default::default()
        };
        assert!(coverage.reaches_the_start() && coverage.last_change_known());
        coverage.too_large.push(group.clone());
        assert!(!coverage.reaches_the_start(), "a group too large to follow");
        assert!(!coverage.last_change_known());
        let mut newest = Coverage::default();
        newest.below(coverage.clone());
        assert_eq!(newest.too_large, [group]);
        assert_eq!(newest.reach, Reach::GraphStart);
        let found = SearchFound {
            creation: Creation::Found(commit(5, "act-old")),
            requests: 7,
        };
        newest.searched(&found);
        assert_eq!(newest.creation, found.creation);
        newest.creation = Creation::InEntries;
        newest.searched(&found);
        assert_eq!(newest.creation, Creation::InEntries, "an entry holds it");
    }
}
