//! One open graph on one server and branch: schema, branches, type counts, and every read and
//! write Studio sends for it (spec 003, 004, 008).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use futures::FutureExt as _;
use futures::channel::oneshot;
use futures::future::{Either, WeakShared, select};
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Subscription, Task};
use gq_language::builder::{Depends, Seeds};
use gq_language::input::same_value;
use gq_language::{BuiltQuery, RelationshipGroup, SchemaModel, builder};
use graph_client::api::{
    AccessAction, BranchCreateOutput, BranchCreateRequest, BranchDeleteOutput, BranchMergeOutput,
    BranchMergeRequest, ChangeFeedOutput, ChangeOutput, ChangeRequest, CommitKind, CommitOutput,
    DiffOutput, EntityKind, GraphListResponse, InvokeStoredQueryRequest, MergeBaseOutput,
    QueryCatalogEntry, QueryRequest, ReadOutput, ReadTargetOutput, ReadinessOutput,
    SnapshotDatasetOutput, SnapshotOutput,
};
use graph_client::{
    ApiErrorSummary, BlobAt, BlobCell, BlobMeta, CommitChanges, FeedAt, GraphClient, ReadError,
    ServerHealth, WriteOutcome,
};
use serde_json::Value;

use crate::access::{Access, AccessScope, Allowed};
use crate::carry::{ChangeLog, ChangesSince, FeedFold, Since, answer_unchanged};
use crate::connection::{Connection, ConnectionEvent, ConnectionWatch};
use crate::degrees::{DEGREE_BATCH, DegreeBatchKey, DegreeBatches, page_counts};
use crate::memo::{Held, Memo};
use crate::scope::{ReadScope, Scope, ScopeSince};

/// A value that loads from the server.
#[derive(Debug, Clone, PartialEq)]
pub enum Loadable<T> {
    NotLoaded,
    Loading,
    Loaded(T),
    Failed(String),
}

impl<T> Loadable<T> {
    pub fn loaded(&self) -> Option<&T> {
        match self {
            Loadable::Loaded(value) => Some(value),
            _ => None,
        }
    }

    pub fn is_loading(&self) -> bool {
        matches!(self, Loadable::Loading)
    }
}

/// How one write ended, kept for the activity log (constitution III).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutcomeKind {
    /// The write landed on its branch: `commit` is the branch's head now, or none when it
    /// published no commit (a branch created or deleted, a merge already up to date, a
    /// mutation that matched nothing).
    Committed {
        commit: Option<String>,
    },
    /// The write landed as `commit`, which no branch's head moved to: a proposal or a snapshot,
    /// as `held` says. The outcome's branch is the one the write was sent to, which did not
    /// change.
    Held {
        commit: String,
        held: HeldAs,
    },
    Refused {
        message: String,
    },
    Unknown {
        reason: String,
    },
}

/// What a write that moved no branch made of its result ([`OutcomeKind::Held`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeldAs {
    /// A proposal, written `as commit` (a mutation or a merge): on no branch until `commit
    /// publish` puts it on the branch it was made for; `commit drop` drops it.
    Proposal,
    /// An unattached snapshot, written `as unattached branch`: read by its id, never published.
    Snapshot,
}

impl OutcomeKind {
    /// How a write that went through ended, by the kind of the commit its answer names (`None`
    /// when it made none): a published commit moved its branch's head, a proposal or a snapshot
    /// moved none, and of a kind this build does not know, whether a branch moved is unknown.
    pub fn written(commit: Option<(String, CommitKind)>) -> Self {
        match commit {
            None => OutcomeKind::Committed { commit: None },
            Some((commit, _)) if commit.is_empty() => OutcomeKind::Committed { commit: None },
            Some((commit, CommitKind::Published)) => OutcomeKind::Committed {
                commit: Some(commit),
            },
            Some((commit, CommitKind::Unpublished)) => OutcomeKind::Held {
                commit,
                held: HeldAs::Proposal,
            },
            Some((commit, CommitKind::Unattached)) => OutcomeKind::Held {
                commit,
                held: HeldAs::Snapshot,
            },
            Some((commit, CommitKind::Other)) => OutcomeKind::Unknown {
                reason: format!(
                    "The server wrote commit {commit}, of a kind Studio does not know, so whether \
                     a branch moved is not known"
                ),
            },
        }
    }
}

/// A write [`GraphSession::external_write`] counts while it runs. It can travel to the task that
/// sends it, off the main thread.
#[must_use = "a write dropped without its outcome is recorded as unknown"]
pub struct ExternalWrite {
    done: oneshot::Sender<OutcomeKind>,
}

impl ExternalWrite {
    /// Records how the write ended.
    pub fn finish(self, outcome: OutcomeKind) {
        self.done.send(outcome).ok();
    }
}

/// Which property a draft is for: the node's type, its id and the property's name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DraftKey {
    pub type_name: String,
    pub id: String,
    pub property: String,
}

/// A property's unsaved value (spec 014): the value it was read at, which its save is bound to,
/// and the text typed for it.
#[derive(Debug, Clone, PartialEq)]
pub struct DraftEntry {
    pub old: Value,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct OutcomeEntry {
    pub at: SystemTime,
    /// The branch the write went to: a merge's target.
    pub branch: String,
    /// What the write did, in words, for display alone: no logic reads a fact out of it.
    pub description: String,
    pub outcome: OutcomeKind,
    /// What a committed write's toast offers instead of opening the commit.
    pub follow_up: Option<FollowUp>,
    /// A merge Studio sent ([`GraphSession::merge`]): its source, the heads it was sent over and
    /// what the target's history showed of it since. Its target is [`Self::branch`] and how it
    /// ended [`Self::outcome`].
    pub merge: Option<MergeEntry>,
}

impl OutcomeEntry {
    /// A merge whose outcome is unknown and which the target's history has not settled: it may
    /// have landed, or may still land, so no review of its branches offers Merge, or a fix,
    /// meanwhile.
    pub fn merge_may_land(&self) -> bool {
        matches!(self.outcome, OutcomeKind::Unknown { .. })
            && self
                .merge
                .as_ref()
                .is_some_and(|merge| merge.settled.is_none())
    }
}

/// A merge as its outcome entry keeps it ([`OutcomeEntry::merge`]). One whose outcome is
/// unknown is settled by reading the target's history, never by merging again (constitution
/// III): the read needs the heads it was sent over, and what it shows is recorded here, so
/// every review of the same branches in the session starts from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeEntry {
    /// The branch merged.
    pub source: String,
    pub sent: MergeSent,
    /// What the target's history showed of a merge whose outcome is unknown, once a read could
    /// tell ([`GraphSession::settle_merge`]); `None` until then.
    pub settled: Option<MergeSettled>,
}

/// The heads a merge was sent over: the source head it merges, and the target's head as Studio
/// read it just before sending the merge. A merge that lands publishes a commit of the target
/// above that head, so the target's history is read for it only down to there.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergeSent {
    pub source_head: Option<String>,
    pub target_head: Option<String>,
}

/// What the target's history showed of a merge whose outcome is unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeSettled {
    /// The history holds the merge: it landed.
    Landed,
    /// The history read, down to the target's head before the merge or to its end, does not
    /// hold it. The merge may still land, but nothing shows it, so Merge may run again.
    NotShown,
}

/// A follow-up a write's outcome offers once it commits (spec 002, toasts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FollowUp {
    /// The commit only removed rows, and Studio can put them back (spec 004).
    UndoDelete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    SchemaChanged,
    BranchChanged,
    BranchesChanged,
    CountsChanged,
    /// A commit landed on `branch`: one of Studio's writes, or another writer's that the head
    /// watch saw. The branch's newest commit is read again.
    Committed {
        branch: String,
    },
    /// `branch`'s head is now `head` ([`GraphSession::head`]), seen for the first time or
    /// moved. A view pinned to another commit of the branch may offer Refresh.
    HeadMoved {
        branch: String,
        head: String,
    },
    ActivityChanged,
    /// The graph became unavailable (503) or answered again.
    AvailabilityChanged,
    /// The graph's stored queries were read ([`GraphSession::stored_queries`]).
    StoredQueriesChanged,
    /// A draft was kept, changed or let go ([`GraphSession::set_draft`]).
    DraftsChanged,
    /// `branch`'s change feed was read up to its head, or the read stopped: a view waiting on
    /// [`GraphSession::changes_since`] asks again.
    ChangesRead {
        branch: String,
    },
    /// Another writer changed the schema, seen at a head move or when the server refused the
    /// head probe. The session reads under the new one now; a view still showing what it read
    /// under the old one offers Refresh, which takes it. Unlike
    /// [`SessionEvent::SchemaChanged`], nothing on screen changes by itself.
    SchemaMoved,
    /// The session's branch is not in a branch list read after it moved there (deleted
    /// elsewhere, or since the last run), so the session moved to main.
    BranchMissing {
        branch: String,
    },
    /// What the credential may do was read, or is to be read again ([`GraphSession::allows`]).
    AccessChanged,
    /// The connection's token changed, and the session reads under the new one: a view that
    /// shows what it read of the server itself (the storage inspector) reads it again, since
    /// the new token may see less. A parked graph says so when shown again.
    CredentialChanged,
}

/// How many answers to reads pinned to a commit, and Blob cells at a commit, a session keeps. A
/// node page holds one per relationship group, up to about a hundred; the answers together are
/// kept within [`PINNED_READ_BYTES`], least recently used first out.
const PINNED_READS: usize = 4_096;
const PINNED_READ_BYTES: usize = 64 * 1024 * 1024;
const PINNED_BLOBS: usize = 2_048;

/// How many node answers a page's rows give ([`Seeds`]) a session keeps, apart from the reads
/// it sent: a table page seeds one per row, so scrolling twenty pages would otherwise push out
/// relationship groups that took seconds to read. A seed costs nothing to make again: the
/// page holding it is read again, or the node alone.
const SEEDED_READS: usize = 1_024;
const SEEDED_READ_BYTES: usize = 16 * 1024 * 1024;

/// How many branches' newest commits a session keeps.
const HEAD_COMMITS: usize = 64;
/// Pages of commits' changes kept, and what they may weigh together.
const COMMIT_CHANGE_PAGES: usize = 256;
const COMMIT_CHANGE_BYTES: usize = 32 * 1024 * 1024;

/// Ids per read of [`GraphSession::node_names`], and the most nodes one call names: a review
/// of 20,000 changes asks a bounded number of small reads, and an end past the limit keeps its
/// id.
const NAME_BATCH: usize = 500;
const NAME_LIMIT: usize = 5_000;

/// How often a session asks after the heads of the branches its views show, while a window of
/// Studio's is active.
const HEAD_POLL: Duration = Duration::from_secs(30);

/// Changes asked for per page of a change-feed read, and the most pages one read takes. A read
/// that stops sooner leaves its remaining commits unknown: the views over them read again.
const FEED_PAGE: u32 = 1_000;
const FEED_PAGES: usize = 8;

/// A view's interest in a branch's head: the session asks after the branch while one is held.
/// The session's own branch is always asked after.
pub struct HeadWatch {
    _alive: Rc<()>,
}

/// Where a new branch starts ([`GraphSession::create_branch`]): a branch's head as the branch
/// is created, or a published commit, with that commit's data and history. The server takes
/// one or the other, never both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchStart {
    Head(String),
    Commit(String),
}

impl BranchStart {
    fn request(&self, name: String) -> BranchCreateRequest {
        let (from, from_commit) = match self {
            BranchStart::Head(branch) => (Some(branch.clone()), None),
            BranchStart::Commit(commit) => (None, Some(commit.clone())),
        };
        BranchCreateRequest {
            name,
            from,
            from_commit,
        }
    }
}

/// The server as the storage inspector shows it ([`GraphSession::server_state`]): each read's
/// own answer, so one failing leaves the others.
pub struct ServerState {
    pub health: Result<ServerHealth, ReadError>,
    pub readiness: Result<ReadinessOutput, ReadError>,
    pub graphs: Result<GraphListResponse, ReadError>,
    pub snapshot: Result<SnapshotOutput, ReadError>,
}

/// A read at a commit: the commit, the query text and its parameters, and the schema epoch it
/// was read under ([`GraphSession::schema_epoch`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PinnedRead {
    commit: String,
    query: String,
    params: String,
    schema: u64,
}

impl PinnedRead {
    /// The key of `query` read at `commit` under schema epoch `schema`.
    fn of(query: &BuiltQuery, commit: &str, schema: u64) -> Self {
        Self {
            commit: commit.to_owned(),
            query: query.text.clone(),
            params: params_key(&query.params),
            schema,
        }
    }
}

/// One node's count through a relationship group, at a commit and schema epoch.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DegreeAt {
    type_name: String,
    group: RelationshipGroup,
    id: String,
    commit: String,
    schema: u64,
}

/// How many nodes' counts a session keeps.
const DEGREE_COUNTS: usize = 16_384;

/// How many writes' outcomes the activity log keeps: an agent can write thousands of times in
/// one session.
const OUTCOMES: usize = 1_000;
/// The most deleted branches remembered so a late save keeps no draft on them.
const GONE_BRANCHES: usize = 256;

/// Adds `commit` to the commits kept for `family`, the newest last and the last 16 at most. A
/// new family past `cap` takes the place of another: carrying across commits is a saving, not
/// a need, and a table's scroll seeds a family per row, so emptying the map at the cap would
/// forget every other read's commits at once.
fn remember<F: std::hash::Hash + Eq + Clone>(
    map: &mut HashMap<F, Vec<String>>,
    family: F,
    commit: &str,
    cap: usize,
) {
    if map.len() >= cap
        && !map.contains_key(&family)
        && let Some(other) = map.keys().next().cloned()
    {
        map.remove(&other);
    }
    let commits = map.entry(family).or_default();
    if commits.last().map(String::as_str) != Some(commit) {
        commits.retain(|kept| kept != commit);
        commits.push(commit.to_owned());
        if commits.len() > 16 {
            commits.remove(0);
        }
    }
}

/// A query's parameters as a memo key holds them: as sent.
fn params_key(params: &serde_json::Map<String, Value>) -> String {
    if params.is_empty() {
        String::new()
    } else {
        Value::Object(params.clone()).to_string()
    }
}

/// The first wait before a graph that answered 503 is asked again; it doubles up to the cap.
const RECHECK_FIRST: Duration = Duration::from_secs(5);
const RECHECK_CAP: Duration = Duration::from_secs(60);

/// What an answer kept at an earlier commit of `log`'s branch gives for commit `to`: walks back
/// from `to` past `kept`, the commits a family of answers was kept at, the newest first, and
/// gives what `take` makes of the first that stands for `to` over the range since it. A commit
/// whose answer is no longer held (`held` says) leaves `kept` first, so the walk never goes
/// back for it. Only commits the log ranks below `to` are looked for, and the walk ends once
/// past the oldest of them: a miss costs the commits between `to` and that one, not the log.
fn carry_back<T>(
    log: Option<&ChangeLog>,
    to: &str,
    kept: &mut Vec<String>,
    held: impl Fn(&str) -> bool,
    mut take: impl FnMut(&str, &ChangesSince) -> Option<T>,
) -> Option<T> {
    kept.retain(|commit| commit == to || held(commit));
    let log = log?;
    let to_rank = log.rank(to)?;
    let mut remaining: HashMap<&str, u64> = kept
        .iter()
        .filter_map(|commit| {
            let rank = log.rank(commit).filter(|rank| *rank < to_rank)?;
            Some((commit.as_str(), rank))
        })
        .collect();
    let mut oldest = remaining.values().copied().min()?;
    let mut found = None;
    // One walk back from `to` gives the range since each kept commit it passes.
    log.since_each(to, |start, since| {
        if remaining.remove(start).is_some() {
            found = take(start, since);
            match remaining.values().copied().min() {
                Some(next) if found.is_none() => oldest = next,
                _ => return true,
            }
        }
        // The walk meets commits in falling rank: below the oldest kept, it meets none.
        log.rank(start).is_some_and(|rank| rank < oldest)
    });
    found
}

/// The node reads a page of `type_name`'s nodes answers, each with its key at the page's commit
/// under schema epoch `epoch`: the read of one node by its id, answered by its row.
fn node_answers(
    schema: &SchemaModel,
    type_name: &str,
    output: &ReadOutput,
    epoch: u64,
) -> Vec<(PinnedRead, ReadOutput)> {
    let Some(commit) = &output.graph_commit_id else {
        return Vec::new();
    };
    let rows = output
        .rows
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    rows.iter()
        .filter_map(|row| {
            let id = row.get("n")?.get("@id")?.as_str()?;
            let query = builder::node_by_id(schema, type_name, id).ok()?;
            let key = PinnedRead::of(&query, commit, epoch);
            let answer = ReadOutput {
                query_name: query.name,
                target: ReadTargetOutput {
                    branch: None,
                    snapshot: Some(commit.clone()),
                },
                row_count: 1,
                columns: output.columns.clone(),
                rows: Value::Array(vec![row.clone()]).into(),
                graph_commit_id: Some(commit.clone()),
                page: None,
            };
            Some((key, answer))
        })
        .collect()
}

/// What one JSON value takes where it is held: in its array's slot, its object's entry, or the
/// answer's `Arc`.
const VALUE_SIZE: usize = std::mem::size_of::<Value>();
/// One entry of a decoded object: its key's hash, its key and its value. Studio's build keeps
/// objects in their order (serde_json's `preserve_order`, an `IndexMap`), each entry in one
/// vector beside a hash index of positions.
const OBJECT_ENTRY_SIZE: usize =
    std::mem::size_of::<u64>() + std::mem::size_of::<String>() + VALUE_SIZE;

/// About what a JSON value takes in memory as serde_json decodes an answer into one: the value
/// itself and everything it holds ([`value_heap`]). A small value takes several times its text:
/// a page of node rows decodes to about ten times its bytes.
fn value_bytes(value: &Value) -> usize {
    VALUE_SIZE + value_heap(value)
}

/// What a JSON value holds on the heap: each string's bytes, each array's slots, and each
/// object's entries and index, counted as serde_json allocates them. Strings are read to their
/// length; arrays and objects grow one element at a time, so their room is what that leaves.
/// The allocator's own rounding is not counted.
fn value_heap(value: &Value) -> usize {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) => 0,
        Value::String(text) => text.capacity(),
        Value::Array(items) => {
            items.capacity() * VALUE_SIZE + items.iter().map(value_heap).sum::<usize>()
        }
        Value::Object(map) => object_heap(map),
    }
}

/// What a decoded object holds on the heap: its keys and values, and the room [`object_room`]
/// gives its entries.
fn object_heap(map: &serde_json::Map<String, Value>) -> usize {
    object_room(map.len())
        + map
            .iter()
            .map(|(key, item)| key.capacity() + value_heap(item))
            .sum::<usize>()
}

/// The room an object of `len` entries read one at a time has: its index's buckets (a
/// position and a control byte each, and a group of control bytes past the last), and as many
/// entry slots as the index holds positions. The index starts at 4 buckets and doubles when
/// full, holding one fewer position than its buckets below 16 and seven eighths of them from
/// 16 (hashbrown's `bucket_mask_to_capacity`); the entries grow to match it (indexmap's
/// `reserve_entries`).
fn object_room(len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let holds = |buckets: usize| {
        if buckets < 16 {
            buckets - 1
        } else {
            buckets / 8 * 7
        }
    };
    let mut buckets = 4;
    while holds(buckets) < len {
        buckets *= 2;
    }
    holds(buckets) * OBJECT_ENTRY_SIZE + buckets * (std::mem::size_of::<usize>() + 1) + 16
}

/// About what an answer takes in memory: its rows (an `Arc` of their value), and its columns'
/// names.
fn read_bytes(output: &ReadOutput) -> usize {
    256 + 2 * std::mem::size_of::<usize>()
        + value_bytes(&output.rows)
        + output.columns.capacity() * std::mem::size_of::<String>()
        + output.columns.iter().map(String::capacity).sum::<usize>()
}

/// About what a page of a commit's changes takes: each change's id and its images' values.
fn commit_changes_bytes(page: &CommitChanges) -> usize {
    let image = |image: &Option<graph_client::api::ChangeImageOutput>| {
        image.as_ref().map_or(0, |image| {
            object_heap(&image.properties) + image.endpoints.as_ref().map_or(0, value_bytes)
        })
    };
    256 + page.changes.capacity() * std::mem::size_of::<graph_client::api::EntityChangeOutput>()
        + page
            .changes
            .iter()
            .map(|change| {
                change.id.capacity()
                    + change.entity_type.id.capacity()
                    + change.entity_type.name.capacity()
                    + image(&change.before)
                    + image(&change.after)
            })
            .sum::<usize>()
}

fn parse_schema(source: &str) -> Loadable<Arc<SchemaModel>> {
    match SchemaModel::parse(source) {
        Ok(model) => Loadable::Loaded(Arc::new(model)),
        Err(error) => Loadable::Failed(format!("Studio could not read the schema: {error}")),
    }
}

/// Where the next page of a change-feed read starts.
enum FeedNext {
    Start(String),
    Cursor(String),
    Page(String),
}

impl FeedNext {
    fn at(&self) -> FeedAt<'_> {
        match self {
            Self::Start(start) => FeedAt::Start(start),
            Self::Cursor(cursor) => FeedAt::Cursor(cursor),
            Self::Page(token) => FeedAt::Page(token),
        }
    }
}

/// Answers kept by key, with the read under way shared by everyone who asks meanwhile.
type ReadMemo<K, V> = Memo<K, V, Pending<V>>;

/// A read under way, as a read memo keeps it: a weak handle, so the read stops once every view
/// that asked for it has dropped it (`counted_read` then gives its slot back to the reads still
/// wanted), and a ticket, so its answer is filed for it alone (`Memo::finish`).
#[derive(Clone)]
pub(crate) struct Pending<V: 'static> {
    read: WeakShared<Task<Result<V, ReadError>>>,
    ticket: u64,
}

/// Wanted while some view's task still holds the read: dropping every asker's task stops it,
/// and nothing would file its answer.
impl<V: Clone + 'static> crate::memo::Pend for Pending<V> {
    fn wanted(&self) -> bool {
        self.read.upgrade().is_some()
    }
}

pub struct GraphSession {
    pub connection: Entity<Connection>,
    pub graph_id: String,
    pub branch: String,
    pub schema: Loadable<Arc<SchemaModel>>,
    pub branches: Loadable<Vec<String>>,
    pub datasets: Loadable<Vec<SnapshotDatasetOutput>>,
    /// Each type's row count from `datasets`, by name: the sidebar asks for every type each
    /// frame.
    /// Row counts by kind and type: a node type and an edge type may share a name.
    dataset_counts: HashMap<(EntityKind, String), i64>,
    /// The `__manifest` version of the snapshot `dataset_counts` came from (its
    /// `graph_manifest_version`), held and let go with them.
    counts_version: Option<u64>,
    /// The commit the counts are for ([`Self::counts_commit`]): the branch's newest commit, once
    /// a read of it found it at `counts_version`. Let go with the counts.
    counts_commit: Option<String>,
    /// That read of the branch's newest commit, while under way.
    _counts_commit_task: Option<Task<()>>,
    /// The queries the server serves by name for this graph (spec 007): `NotLoaded` until a view
    /// first wants them ([`Self::want_stored_queries`]), since the first screen shows none.
    pub stored_queries: Loadable<Arc<Vec<QueryCatalogEntry>>>,
    /// A view wanted the catalog ([`Self::want_stored_queries`]): it is read with the graph from
    /// then on, at a reload, a recheck and a new token.
    stored_queries_wanted: bool,
    /// Every write's outcome, oldest first: the newest [`OUTCOMES`].
    pub outcomes: Vec<OutcomeEntry>,
    /// How many outcomes were ever logged, those dropped from [`Self::outcomes`] included: a
    /// view that has seen this many has seen every outcome.
    pub outcomes_logged: u64,
    pub reads_in_flight: usize,
    pub writes_in_flight: usize,
    /// Why the graph is unavailable, when it answered 503 (spec 003); reads resume, and this
    /// clears, when it answers again.
    pub unavailable: Option<String>,
    recheck_after: Duration,
    /// Answers to reads pinned to a commit, and those under way (they never change).
    pinned_reads: ReadMemo<PinnedRead, ReadOutput>,
    /// Node answers a page's rows gave at its commit ([`Self::seed`]), kept within their own
    /// bound ([`SEEDED_READS`]) so they never push out an answer that was read.
    seeded_reads: Memo<PinnedRead, ReadOutput, ()>,
    /// Numbers the reads the memos keep under way (`Pending`).
    next_ticket: u64,
    pinned_blobs: ReadMemo<(BlobCell, String, u64), BlobMeta>,
    /// Each node's count through a group, by commit: read in a batch, or what a page's counts
    /// gave (`builder::Seeds`). Shared by the table, the peek, the tree and the canvas.
    degree_counts: Memo<DegreeAt, u64, ()>,
    /// The commits each read's answer is kept at, by its query, parameters and schema epoch,
    /// and each node's count: where a miss at one commit looks for the same read at another,
    /// rather than going through every answer kept. Entries the memo let go are skipped and
    /// dropped as they are met.
    pinned_commits: HashMap<(String, String, u64), Vec<String>>,
    /// The commits a page's rows gave each node answer at, as `pinned_commits` for the
    /// answers read, within the seeds' own bound.
    seeded_commits: HashMap<(String, String, u64), Vec<String>>,
    degree_commits: HashMap<(String, RelationshipGroup, String, u64), Vec<String>>,
    /// Which schema the session reads under: a new number each time the schema's text changes,
    /// the first one included. A read at an old commit uses the current schema, so the same
    /// query at the same commit can mean something else after a rename, a drop or an addition;
    /// every answer kept is kept under its epoch, and nothing crosses one.
    schema_epoch: u64,
    schema_text: Option<Arc<str>>,
    /// Branches' newest commits, read alone for a list of branches, and kept until a commit
    /// lands on the branch.
    head_commits: ReadMemo<String, Option<CommitOutput>>,
    /// Pages of commits' changes, by commit, page token and size: a commit never changes, so
    /// the views that show one (History's panel and tab, its detail) read each page once.
    commit_change_pages: ReadMemo<(String, Option<String>, u32), CommitChanges>,
    /// For a commit's page (by its token) that came back whole, the limit it was asked with:
    /// a page holding all the changes answers a larger ask as well, so a History row's count
    /// answers the commit view beside it, and a one-commit review. Kept to the pages held.
    whole_commit_pages: HashMap<(String, Option<String>), u32>,
    /// The branch list has been read once; reading it again forgets the head commits of the
    /// branches that came or went.
    branches_read: bool,
    /// How many times the session has moved to another branch: a branch list asked for before
    /// the last move may not hold a branch made since, so only a later one shows it is gone.
    branch_moves: u64,
    /// Relationship counts asked for this turn, read together when it ends.
    degrees: DegreeBatches,
    /// The commit each watched branch's head was last seen at, by the head watch or as one of
    /// Studio's own commits.
    heads: HashMap<String, String>,
    /// One of Studio's own commits on each branch that did not land on the head held, while a
    /// head read says which is newer ([`Self::saw_head`]): found at the head, it is that commit,
    /// not another writer's.
    placing: HashMap<String, String>,
    /// Unsaved property values by branch (spec 014): every table, page and peek of the graph
    /// reads and writes these, and a branch's drafts wait for it while another is shown.
    drafts: HashMap<String, BTreeMap<DraftKey, DraftEntry>>,
    /// Branches deleted since a draft was last typed on one of their name: a write or read that
    /// settles after the deletion keeps no draft there ([`Self::settle_draft_on`]).
    gone_branches: HashSet<String>,
    /// Branches views asked after besides the session's own, while their watch is held.
    head_watches: Vec<(String, Weak<()>)>,
    /// Head reads under way, by branch.
    head_reads: HashMap<String, Task<()>>,
    /// Views waiting for a branch's head read to land ([`Self::head_now`]).
    head_waiters: HashMap<String, Vec<oneshot::Sender<()>>>,
    /// Studio's own data commits on top of the head held, by branch, until a feed read went
    /// past them: the schema is read after a feed read unless every commit it read is one.
    own_heads: HashMap<String, HashSet<String>>,
    /// Branches whose head moved since a change read last took them other than by one of
    /// Studio's own data commits on top of the head held: a read that cannot tell every commit
    /// it skips (nobody reads the branch, or it stopped short) compares the schema then.
    foreign_moves: HashSet<String>,
    /// What each view has read at the commit it shows ([`ReadScope`]), while it holds its scope.
    scopes: HashMap<u64, Scope>,
    next_scope: u64,
    /// Each followed branch's commits as the change feed described them.
    change_logs: HashMap<String, ChangeLog>,
    /// Change-feed reads under way, by branch, and the branches whose head moved meanwhile.
    change_reads: HashMap<String, Task<()>>,
    change_reads_again: HashSet<String>,
    /// The branch list read again by the head watch, while under way.
    branch_recheck: Option<Task<()>>,
    /// A poll came while no window was active; the next activation asks at once.
    head_check_missed: bool,
    _head_poll: Option<Task<()>>,
    _recheck: Option<Task<()>>,
    /// The branches watched at the last head check: one no longer watched has its moves go
    /// unseen from then, so its head commit is read again when next asked for.
    watched: Vec<String>,
    /// A read of the schema the person asked for is under way: a recheck in its place would
    /// cancel it. A reason to check that comes meanwhile is checked once it lands
    /// (`recheck_schema_again`).
    reloading_schema: bool,
    /// A recheck of the schema under way ([`Self::recheck_schema`]), and whether its read has
    /// left: one at a time, so a reason to check met meanwhile neither cancels it nor sends
    /// another. One met after the read left checks once more when it is back
    /// (`recheck_schema_again`), since that read may have missed what moved.
    schema_recheck: Option<Arc<AtomicBool>>,
    recheck_schema_again: bool,
    /// The property chosen to name each type's nodes in this graph (spec 002, "Node names"),
    /// as the settings held it when the schema was last named by it.
    labels: BTreeMap<String, String>,
    _settings: Option<Subscription>,
    _schema_task: Option<Task<()>>,
    _branches_task: Option<Task<()>>,
    _datasets_task: Option<Task<()>>,
    /// A counts read is under way, and commits seen meanwhile want one more after it.
    counts_reading: bool,
    counts_again: bool,
    _stored_queries_task: Option<Task<()>>,
    /// What the credential may do (spec 002), by the branch asked about: read when the session
    /// opens and moves to a branch, and again after a 403 or a new token.
    access: Access,
    /// The client's credential count ([`GraphClient::credential_changes`]) the answers kept were
    /// read under. A new token sees the graph as its own policy allows, which may be less than
    /// the old one did, so nothing read with the old one is answered from again; an answer
    /// still out when it changed is kept nowhere.
    credential: u64,
    _connection_events: Option<Subscription>,
    /// Whether a window shows the graph ([`Self::set_shown`]): a parked one asks after no head
    /// and no branch list, and holds no watch on its server.
    shown: bool,
    /// The first reads were sent: the schema, the branch list, the type counts, what the
    /// credential may do, and the stored-query catalog once wanted. A session opened behind the
    /// graph its window shows ([`Self::parked`]) sends them when it is first shown, and a shown
    /// one when its connection's token is ready ([`Connection::token_ready`]).
    opened: bool,
    /// The wait after a 503 ended while the graph was parked: it is asked again when shown.
    recheck_owed: bool,
    /// The settings named a type's nodes otherwise while the graph was parked: views hear it
    /// ([`SessionEvent::SchemaChanged`]) when it is shown again.
    names_owed: bool,
    /// Numbers the branch-list reads as they are sent, and the newest whose list was taken: a
    /// list asked for before one already taken answers for an older moment, and is not taken.
    branch_lists_sent: u64,
    branch_list_taken: u64,
    /// A new token came while the graph was parked: its counts, catalog and branch list are
    /// read under it when shown again ([`Self::read_again_for_credential`]).
    credential_owed: bool,
    /// What the credential may do was forgotten while parked: it is read when shown again.
    access_owed: bool,
    _connection_watch: Option<ConnectionWatch>,
}

impl EventEmitter<SessionEvent> for GraphSession {}

impl GraphSession {
    /// A session on `graph_id` that a window shows: it reads the graph at once.
    pub fn new(
        connection: Entity<Connection>,
        graph_id: String,
        branch: String,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut session = Self::parked(connection, graph_id, branch, cx);
        session.set_shown(true, cx);
        session
    }

    /// A session on `graph_id` opened behind the graph its window shows, as a start reopens
    /// a window's graphs: it sends nothing, not even its first reads, until it is first shown
    /// ([`Self::set_shown`]).
    pub fn parked(
        connection: Entity<Connection>,
        graph_id: String,
        branch: String,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut session = Self {
            connection,
            graph_id,
            branch,
            schema: Loadable::NotLoaded,
            branches: Loadable::NotLoaded,
            datasets: Loadable::NotLoaded,
            dataset_counts: HashMap::new(),
            counts_version: None,
            counts_commit: None,
            _counts_commit_task: None,
            stored_queries: Loadable::NotLoaded,
            stored_queries_wanted: false,
            outcomes: Vec::new(),
            outcomes_logged: 0,
            reads_in_flight: 0,
            writes_in_flight: 0,
            unavailable: None,
            recheck_after: RECHECK_FIRST,
            pinned_reads: Memo::weighed(PINNED_READS, PINNED_READ_BYTES, read_bytes),
            seeded_reads: Memo::weighed(SEEDED_READS, SEEDED_READ_BYTES, read_bytes),
            next_ticket: 0,
            pinned_blobs: Memo::new(PINNED_BLOBS),
            degree_counts: Memo::new(DEGREE_COUNTS),
            pinned_commits: HashMap::new(),
            seeded_commits: HashMap::new(),
            degree_commits: HashMap::new(),
            schema_epoch: 0,
            schema_text: None,
            head_commits: Memo::new(HEAD_COMMITS),
            commit_change_pages: Memo::weighed(
                COMMIT_CHANGE_PAGES,
                COMMIT_CHANGE_BYTES,
                commit_changes_bytes,
            ),
            whole_commit_pages: HashMap::new(),
            branches_read: false,
            branch_moves: 0,
            degrees: DegreeBatches::default(),
            heads: HashMap::new(),
            placing: HashMap::new(),
            drafts: HashMap::new(),
            head_watches: Vec::new(),
            head_reads: HashMap::new(),
            head_waiters: HashMap::new(),
            own_heads: HashMap::new(),
            foreign_moves: HashSet::new(),
            gone_branches: HashSet::new(),
            scopes: HashMap::new(),
            next_scope: 0,
            change_logs: HashMap::new(),
            change_reads: HashMap::new(),
            change_reads_again: HashSet::new(),
            branch_recheck: None,
            head_check_missed: false,
            _head_poll: None,
            _recheck: None,
            watched: Vec::new(),
            reloading_schema: false,
            schema_recheck: None,
            recheck_schema_again: false,
            labels: BTreeMap::new(),
            _settings: None,
            _schema_task: None,
            _branches_task: None,
            _datasets_task: None,
            counts_reading: false,
            counts_again: false,
            _stored_queries_task: None,
            access: Access::default(),
            credential: 0,
            _connection_events: None,
            shown: false,
            opened: false,
            recheck_owed: false,
            names_owed: false,
            branch_lists_sent: 0,
            branch_list_taken: 0,
            credential_owed: false,
            access_owed: false,
            _connection_watch: None,
        };
        session.credential = session.client(cx).credential_changes();
        session._connection_events = Some(cx.subscribe(
            &session.connection,
            |this, _, event: &ConnectionEvent, cx| match event {
                ConnectionEvent::CredentialChanged => {
                    this.sync_access(cx);
                }
                ConnectionEvent::TokenReady => this.open_when_ready(cx),
                ConnectionEvent::StateChanged => {}
            },
        ));
        session.labels = session.chosen_labels(cx);
        session._settings = Some(
            cx.observe_global::<studio_settings::StudioSettings>(|this, cx| {
                this.labels_changed(cx)
            }),
        );
        // The polls stop asking while the graph is parked, and say a poll came.
        session._head_poll = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(HEAD_POLL).await;
                let alive = this
                    .update(cx, |this, cx| {
                        if this.shown && cx.active_window().is_some() {
                            this.check_heads(cx);
                        } else {
                            this.head_check_missed = true;
                        }
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        }));
        session
    }

    /// Whether the credential may take `action` for `scope`, as the access route answered
    /// (spec 002). Studio offers every action not known to be refused; the server decides.
    pub fn allows(&self, action: AccessAction, scope: &AccessScope) -> Allowed {
        self.access.allows(action, scope)
    }

    /// Whether the credential may take `action` on the session's branch.
    pub fn allows_here(&self, action: AccessAction) -> Allowed {
        self.access
            .allows(action, &AccessScope::branch(&self.branch))
    }

    /// The actor the credential resolves to, once the access route has answered.
    pub fn actor(&self) -> Option<&str> {
        self.access.actor()
    }

    /// Reads what the credential may do for `scope`, unless that was read or is being read
    /// since the credential last may have changed; [`SessionEvent::AccessChanged`] says when an
    /// answer lands. A read that fails is asked again the next time, and one answered after the
    /// credential or the policy may have changed lands nowhere: it may be the old one's.
    pub fn read_access(&mut self, scope: AccessScope, cx: &mut Context<Self>) {
        self.sync_access(cx);
        let Some(generation) = self.access.wants(&scope) else {
            return;
        };
        let client = self.client(cx);
        let graph = self.graph_id.clone();
        let (branch, target) = (scope.branch.clone(), scope.target.clone());
        let read = self.counted_read(
            async move {
                client
                    .access(&graph, branch.as_deref(), target.as_deref())
                    .await
            },
            cx,
        );
        cx.spawn(async move |this, cx| {
            let answer = read.await.ok();
            this.update(cx, |this, cx| {
                // A new token, or a 403 on the graph, since it was sent forgets every answer,
                // this one with them.
                this.sync_access(cx);
                if this.access.landed(scope, generation, answer) {
                    cx.emit(SessionEvent::AccessChanged);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Catches up with the client's counts: after a new token every answer kept is forgotten
    /// ([`Self::forget_reads`]) and what the session shows of the graph is read again
    /// ([`Self::read_again_for_credential`]); after a new token or a 403 on the graph what the
    /// credential may do is forgotten too, and the session's branch is read again; views that
    /// show what it allows hear [`SessionEvent::AccessChanged`] and ask for theirs. A parked
    /// graph forgets at once and reads when shown again. Whether anything was forgotten.
    fn sync_access(&mut self, cx: &mut Context<Self>) -> bool {
        let client = self.client(cx);
        let credential = client.credential_changes();
        if credential != self.credential {
            self.credential = credential;
            self.forget_reads();
            self.read_again_for_credential(cx);
        }
        // This graph's refusals alone: a 403 on another graph of the server says nothing of
        // what the credential may do here, and an answer for this graph in flight stands.
        if !self
            .access
            .sync((credential, client.refusals(&self.graph_id)))
        {
            return false;
        }
        // A parked graph asks the server nothing; shown again, it reads what it may do.
        if self.shown {
            self.access_changed(cx);
        } else {
            self.access_owed = true;
        }
        true
    }

    /// What the credential may do was forgotten: views that show it hear so and ask for theirs,
    /// and the session's branch is read again.
    fn access_changed(&mut self, cx: &mut Context<Self>) {
        cx.emit(SessionEvent::AccessChanged);
        cx.notify();
        let branch = AccessScope::branch(&self.branch);
        self.read_access(branch, cx);
    }

    /// Forgets every answer kept, and the reads under way with them, which then keep nothing
    /// when they land: answers read at a commit, Blob cells, relationship counts, branches'
    /// newest commits and commits' changes. Each was read with a credential that may see more
    /// than the one sent now, and a kept answer is shown again without asking the server.
    ///
    /// What it keeps, and why: the schema, which the server answers alike to every credential
    /// that may read the graph and every read is made under (Refresh, or another writer's
    /// schema seen at a head, reads it again); and the heads it watches with each branch's
    /// change log, which describe commits, never the rows a view shows: a commit never changes,
    /// and a log only carries an answer forward, from those read since with the new token.
    fn forget_reads(&mut self) {
        self.pinned_reads.clear();
        self.seeded_reads.clear();
        self.pinned_blobs.clear();
        self.degree_counts.clear();
        self.pinned_commits.clear();
        self.seeded_commits.clear();
        self.degree_commits.clear();
        self.head_commits.clear();
        self.commit_change_pages.clear();
        self.whole_commit_pages.clear();
    }

    /// Reads again, under a new token, what the session shows of the graph as the old one was
    /// answered it: the type counts, the stored-query catalog (once wanted) and the branch
    /// list; and every read a 401 could have refused under the old one: the schema, when none
    /// is held. The new token's policy may refuse it what the old one read, so none of it shows
    /// meanwhile: each is being read, and views hear it, and [`SessionEvent::CredentialChanged`]
    /// tells views that read the server themselves. A branch the new token's list does not hold
    /// is left, as any branch the list no longer holds is; drafts on it stay. A schema held is
    /// kept: the server answers it alike to every credential that may read the graph.
    ///
    /// A parked graph (one no window shows, [`Self::set_shown`]) asks the server nothing: it
    /// lets go of the old token's answers and the reads still out under it, and reads under the
    /// new one when shown again.
    fn read_again_for_credential(&mut self, cx: &mut Context<Self>) {
        // Nothing was read yet: the first show reads it all under the token it then has.
        if !self.opened {
            return;
        }
        self.counts_reading = false;
        self.counts_again = false;
        // Not kept while it is read: it is the old token's.
        self.branches = Loadable::Loading;
        if self.shown {
            // Refused under the old token, or still out under it.
            if !matches!(self.schema, Loadable::Loaded(_)) {
                self.reload_schema(cx);
            }
            self.reload_counts(cx);
            if self.stored_queries_wanted {
                self.reload_stored_queries(cx);
            }
            self.read_branches(None, cx);
        } else {
            self.datasets = Loadable::Loading;
            self.clear_counts();
            if self.stored_queries_wanted {
                self.stored_queries = Loadable::Loading;
            }
            self._datasets_task = None;
            self._stored_queries_task = None;
            self._branches_task = None;
            self.branch_recheck = None;
            if !matches!(self.schema, Loadable::Loaded(_)) {
                self._schema_task = None;
                self.reloading_schema = false;
            }
            self.credential_owed = true;
        }
        cx.emit(SessionEvent::CountsChanged);
        cx.emit(SessionEvent::StoredQueriesChanged);
        // Views read on these, and a parked graph asks the server nothing: History reads its
        // first page for a changed branch list when its last read failed. Shown again, the
        // reads owed run this again, and views hear them then.
        if self.shown {
            cx.emit(SessionEvent::BranchesChanged);
            cx.emit(SessionEvent::CredentialChanged);
        }
        cx.notify();
    }

    /// The client every read and write of this session goes through. Views never hold one
    /// (`docs/architecture.md`): they ask the session, which counts the read and notes what
    /// it says about the server.
    pub(crate) fn client(&self, cx: &App) -> GraphClient {
        self.connection.read(cx).client().clone()
    }

    /// The server's address, for a view that shows it.
    pub fn server_url(&self, cx: &App) -> String {
        self.connection.read(cx).client().base_url().to_owned()
    }

    /// The client the agent gateway forwards the agent CLI's own requests through, inside the
    /// sandbox the gateway enforces. The one client a crate outside `graph_store` holds: the
    /// CLI's reads are its own, not the session's.
    pub fn gateway_client(&self, cx: &App) -> GraphClient {
        self.client(cx)
    }

    pub fn server_name(&self, cx: &App) -> String {
        self.connection.read(cx).server.name.clone()
    }

    pub fn schema_model(&self) -> Option<Arc<SchemaModel>> {
        self.schema.loaded().cloned()
    }

    /// Notes what a read says about the graph's availability: a 503 marks it unavailable and
    /// asks again later, with a growing wait, while a window shows it (a parked graph asks
    /// when shown again); any answer clears the mark, and a shown graph then asks after its
    /// heads, which it stopped asking after meanwhile.
    fn note_read<T>(&mut self, result: &Result<T, ReadError>, cx: &mut Context<Self>) {
        match result {
            Err(ReadError::Refused(error)) if error.status == 503 => {
                let first = self.unavailable.is_none();
                self.unavailable = Some(error.message.clone());
                if first || self._recheck.is_none() {
                    let wait = self.recheck_after;
                    self.recheck_after = (wait * 2).min(RECHECK_CAP);
                    self._recheck = Some(cx.spawn(async move |this, cx| {
                        cx.background_executor().timer(wait).await;
                        this.update(cx, |this, cx| {
                            this._recheck = None;
                            if this.shown {
                                this.recheck_available(cx);
                            } else {
                                this.recheck_owed = true;
                            }
                        })
                        .ok();
                    }));
                }
                if first {
                    cx.emit(SessionEvent::AvailabilityChanged);
                }
            }
            Err(ReadError::Transport(_)) => {}
            _ => {
                if self.unavailable.take().is_some() {
                    self.recheck_after = RECHECK_FIRST;
                    self._recheck = None;
                    self.recheck_owed = false;
                    cx.emit(SessionEvent::AvailabilityChanged);
                    if self.shown {
                        self.check_heads(cx);
                    }
                }
            }
        }
    }

    /// Asks a graph that answered 503 again, refreshing what is held rather than letting it
    /// go: the schema, the branch list, the type counts and the stored-query catalog (once
    /// wanted) held stay shown while they are read, and stay if the graph still cannot answer;
    /// what was never read is read.
    fn recheck_available(&mut self, cx: &mut Context<Self>) {
        if matches!(self.schema, Loadable::Loaded(_)) {
            self.recheck_schema(None, cx);
        } else {
            self.reload_schema(cx);
        }
        self.read_branches(None, cx);
        self.refresh_counts(cx);
        if self.stored_queries_wanted {
            self.read_stored_queries(true, cx);
        }
    }

    /// Reloads the schema, the branch list, the type counts and the stored queries (once a view
    /// wanted them), and reads the server's graph list again if it could not be read before.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.reload_schema(cx);
        self.reload_branches(cx);
        self.reload_counts(cx);
        if self.stored_queries_wanted {
            self.reload_stored_queries(cx);
        }
        self.connection
            .update(cx, |connection, cx| connection.list_graphs_again(cx));
    }

    /// A view shows the stored-query catalog (the Queries panel, the palette's queries, a
    /// stored query's tab): it is read now, unless it is held or being read, and with the graph
    /// from then on. A graph not yet read, or parked, reads it with its first reads, or when
    /// shown again. The catalog is not read before: the first screen shows none of it, and on
    /// a graph with many stored queries it is the largest of the first reads.
    pub fn want_stored_queries(&mut self, cx: &mut Context<Self>) {
        self.stored_queries_wanted = true;
        if self.opened && self.shown && matches!(self.stored_queries, Loadable::NotLoaded) {
            self.read_stored_queries(false, cx);
        }
    }

    /// The person's Refresh: [`Self::reload`], and a CLI server's token asked of the CLI again
    /// ([`Connection::refresh_token`]). Only Refresh asks it: a graph first shown reads with the
    /// token its connect was given.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.reload(cx);
        self.connection
            .update(cx, |connection, cx| connection.refresh_token(cx));
    }

    /// Reads the graph's stored-query catalog, which a view wants from then on. A graph
    /// without stored queries answers an empty one; a 404 is a graph the server no longer
    /// serves, and shows as a failure.
    pub fn reload_stored_queries(&mut self, cx: &mut Context<Self>) {
        self.stored_queries_wanted = true;
        self.read_stored_queries(false, cx);
    }

    /// [`Self::reload_stored_queries`], keeping the catalog held while it is read, and when the
    /// read fails, with `keep_held`.
    fn read_stored_queries(&mut self, keep_held: bool, cx: &mut Context<Self>) {
        let keep_held = keep_held && matches!(self.stored_queries, Loadable::Loaded(_));
        if !keep_held {
            self.stored_queries = Loadable::Loading;
        }
        let client = self.client(cx);
        let graph = self.graph_id.clone();
        let read = cx.background_spawn(async move { client.stored_queries(&graph).await });
        self._stored_queries_task = Some(cx.spawn(async move |this, cx| {
            let read = read.await;
            this.update(cx, |this, cx| this.note_read(&read, cx)).ok();
            let result = match read {
                Ok(output) => Loadable::Loaded(Arc::new(output.queries)),
                // The catalog held is still the graph's: a failed read says nothing of it.
                Err(_) if keep_held => return,
                Err(error) => Loadable::Failed(error.to_string()),
            };
            this.update(cx, |this, cx| {
                this.stored_queries = result;
                cx.emit(SessionEvent::StoredQueriesChanged);
                cx.notify();
            })
            .ok();
        }));
    }

    /// The stored query named `name`, once the catalog is read.
    pub fn stored_query(&self, name: &str) -> Option<QueryCatalogEntry> {
        self.stored_queries
            .loaded()?
            .iter()
            .find(|query| query.name == name)
            .cloned()
    }

    /// Runs a stored read at `snapshot`, else at the session's branch head. The server refuses a
    /// stored query that writes, since the request says it expects a read.
    pub fn invoke_stored_read(
        &mut self,
        name: &str,
        params: Option<Value>,
        snapshot: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<ReadOutput, ReadError>> {
        let request = InvokeStoredQueryRequest {
            params,
            branch: snapshot.is_none().then(|| self.branch.clone()),
            snapshot,
            expect_mutation: Some(false),
        };
        let client = self.client(cx);
        let graph = self.graph_id.clone();
        let name = name.to_owned();
        self.counted_read(
            async move { client.invoke_stored_read(&graph, &name, &request).await },
            cx,
        )
    }

    /// Runs a stored mutation on `branch`, sent once and logged like every write; with
    /// `if_commit` it lands only while that commit is the branch head.
    pub fn invoke_stored_write(
        &mut self,
        name: &str,
        params: Option<Value>,
        branch: String,
        if_commit: Option<String>,
        description: String,
        cx: &mut Context<Self>,
    ) -> Task<WriteOutcome<ChangeOutput>> {
        let client = self.client(cx);
        let graph = self.graph_id.clone();
        let name = name.to_owned();
        let request = InvokeStoredQueryRequest {
            params,
            branch: Some(branch.clone()),
            snapshot: None,
            expect_mutation: Some(true),
        };
        let send = async move {
            client
                .invoke_stored_write(&graph, &name, &request, if_commit.as_deref())
                .await
        };
        self.run_write(
            send,
            |output: &ChangeOutput| output.commit.as_ref(),
            move |_, _, _| WriteLog::new(branch, description),
            |_, _, _| {},
            cx,
        )
    }

    /// Reads the schema again. One held stays while it is read, so views keep drawing it, and
    /// stays if it cannot be read: a failed read says nothing about the schema.
    pub fn reload_schema(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.schema, Loadable::Loaded(_)) {
            self.schema = Loadable::Loading;
        }
        self.reloading_schema = true;
        // Takes a recheck's place: this read is sent after whatever asked for one.
        self.schema_recheck = None;
        self.recheck_schema_again = false;
        let client = self.client(cx);
        let graph = self.graph_id.clone();
        // Read and compiled off the main thread: a large schema takes a while to compile.
        let read = cx.background_spawn(async move {
            let read = client.schema(&graph).await;
            let parsed = read
                .as_ref()
                .ok()
                .map(|output| parse_schema(&output.schema_source));
            (read.map(|_| ()), parsed)
        });
        self._schema_task = Some(cx.spawn(async move |this, cx| {
            let (read, parsed) = read.await;
            this.update(cx, |this, cx| this.note_read(&read, cx)).ok();
            let result = match (read, parsed) {
                (Ok(()), Some(parsed)) => parsed,
                (Err(error), _) => Loadable::Failed(error.to_string()),
                (Ok(()), None) => Loadable::Failed("The schema could not be read.".into()),
            };
            this.update(cx, |this, cx| {
                this.reloading_schema = false;
                this.install_schema(result, cx);
                // A reason to check that came while the read was out may postdate its answer.
                if std::mem::take(&mut this.recheck_schema_again) {
                    this.recheck_schema(None, cx);
                }
            })
            .ok();
        }));
    }

    /// What names `type_name`'s nodes in this graph, and what can (spec 002, "Node names").
    pub fn type_naming(&self, type_name: &str, cx: &App) -> Option<studio_settings::TypeNaming> {
        let schema = self.schema_model()?;
        let node = schema.node(type_name)?;
        Some(studio_settings::TypeNaming {
            graph: self.overrides_key(cx),
            type_name: type_name.to_owned(),
            chosen: node
                .label
                .clone()
                .filter(|property| node.property(property).is_some_and(|p| p.can_name())),
            default: node.default_label_property().unwrap_or_default().to_owned(),
            candidates: node.label_candidates().map(|p| p.name.clone()).collect(),
        })
    }

    /// The property chosen to name each type's nodes in this graph, in the settings.
    fn chosen_labels(&self, cx: &App) -> BTreeMap<String, String> {
        match cx.try_global::<studio_settings::StudioSettings>() {
            Some(settings) => settings.labels_for(&self.overrides_key(cx)),
            None => BTreeMap::new(),
        }
    }

    /// `model` naming each type's nodes as this graph chose.
    fn named(&self, model: Arc<SchemaModel>) -> Arc<SchemaModel> {
        if model
            .nodes
            .iter()
            .all(|node| node.label.as_ref() == self.labels.get(&node.name))
        {
            model
        } else {
            Arc::new(model.with_labels(&self.labels))
        }
    }

    /// The settings changed. When they name this graph's nodes otherwise, views take the schema
    /// again at once ([`SessionEvent::SchemaChanged`]), so a node has one name everywhere.
    fn labels_changed(&mut self, cx: &mut Context<Self>) {
        let labels = self.chosen_labels(cx);
        self.take_labels(labels, cx);
    }

    /// Names each type's nodes by `labels`. Views hear it at once, and read what they show
    /// again under the new names; a parked graph's views hear it when it is shown again, since
    /// a parked graph asks the server nothing.
    pub(crate) fn take_labels(&mut self, labels: BTreeMap<String, String>, cx: &mut Context<Self>) {
        if labels == self.labels {
            return;
        }
        self.labels = labels;
        if let Loadable::Loaded(model) = &self.schema {
            self.schema = Loadable::Loaded(self.named(model.clone()));
            if self.shown {
                cx.emit(SessionEvent::SchemaChanged);
                cx.notify();
            } else {
                self.names_owed = true;
            }
        }
    }

    /// Takes a schema read from the server at the person's asking, or the first: views take it
    /// at once ([`SessionEvent::SchemaChanged`]).
    fn install_schema(&mut self, schema: Loadable<Arc<SchemaModel>>, cx: &mut Context<Self>) {
        let schema = match schema {
            Loadable::Loaded(model) => Loadable::Loaded(self.named(model)),
            other => other,
        };
        match &schema {
            Loadable::Loaded(model) => self.start_schema_epoch(model),
            // The schema held is still the graph's.
            _ if matches!(self.schema, Loadable::Loaded(_)) => return,
            _ => {}
        }
        self.schema = schema;
        cx.emit(SessionEvent::SchemaChanged);
        cx.notify();
        self.check_heads(cx);
    }

    /// A text other than the one held starts a new schema epoch: what was kept under the old
    /// one, answers and change logs, is let go.
    fn start_schema_epoch(&mut self, model: &SchemaModel) {
        if self.schema_text.as_deref() == Some(&*model.source) {
            return;
        }
        self.schema_epoch += 1;
        self.schema_text = Some(model.source.clone());
        self.pinned_reads.clear();
        self.seeded_reads.clear();
        self.pinned_blobs.clear();
        // A change names its type as the schema does now.
        self.commit_change_pages.clear();
        self.whole_commit_pages.clear();
        self.degree_counts.clear();
        self.pinned_commits.clear();
        self.seeded_commits.clear();
        self.degree_commits.clear();
        // Each log starts again at its branch's head, so the next move is read; the schema was
        // just read, so no move before it is left to compare.
        self.change_logs = self
            .heads
            .iter()
            .map(|(branch, head)| (branch.clone(), ChangeLog::starting_at(head.clone())))
            .collect();
        self.own_heads.clear();
        self.foreign_moves.clear();
    }

    /// Reads the schema again while keeping the one held, and takes it if its text changed: a
    /// head move, or a head probe the server refused, showed another writer may have changed
    /// it. `read` is a text read already, which is taken without asking again.
    fn recheck_schema(
        &mut self,
        read: Option<graph_client::api::SchemaOutput>,
        cx: &mut Context<Self>,
    ) {
        // A reload under way reads the schema too, and views take it at once; a recheck in its
        // place would cancel it. The reason is checked once it lands, as for a recheck sent.
        if self.reloading_schema {
            self.recheck_schema_again = true;
            return;
        }
        // One at a time. A read not yet sent answers for this reason too; one sent may have
        // been answered before what moved, so the schema is asked once more after it.
        if let Some(sent) = &self.schema_recheck {
            if sent.load(Ordering::SeqCst) {
                self.recheck_schema_again = true;
            }
            return;
        }
        // A text read already was read before this reason to check, as a read sent is.
        let sent = Arc::new(AtomicBool::new(read.is_some()));
        self.schema_recheck = Some(sent.clone());
        self.recheck_schema_again = false;
        let client = self.client(cx);
        let graph = self.graph_id.clone();
        let known = self.schema_text.clone();
        let read = cx.background_spawn(async move {
            let read = match read {
                Some(output) => Ok(output),
                None => {
                    sent.store(true, Ordering::SeqCst);
                    client.schema(&graph).await
                }
            };
            // Compiled off the main thread, and only when it is another text.
            let parsed = read
                .as_ref()
                .ok()
                .filter(|output| known.as_deref() != Some(output.schema_source.as_str()))
                .map(|output| parse_schema(&output.schema_source));
            (read.map(|_| ()), parsed)
        });
        self._schema_task = Some(cx.spawn(async move |this, cx| {
            let (read, parsed) = read.await;
            this.update(cx, |this, cx| {
                this.schema_recheck = None;
                let again = std::mem::take(&mut this.recheck_schema_again);
                this.note_read(&read, cx);
                if let Some(Loadable::Loaded(model)) = parsed
                    && this.schema_text.as_deref() != Some(&*model.source)
                {
                    // The session reads under it from now; views take it on Refresh.
                    this.start_schema_epoch(&model);
                    this.schema = Loadable::Loaded(this.named(model));
                    cx.emit(SessionEvent::SchemaMoved);
                    cx.notify();
                    // The heads are asked after under it: the server refuses a head probe of
                    // a type the old one named and this one renamed or dropped.
                    this.check_heads(cx);
                }
                // A recheck the heads started since is sent after the reasons met meanwhile.
                if again && this.schema_recheck.is_none() {
                    this.recheck_schema(None, cx);
                }
            })
            .ok();
        }));
    }

    /// Reads the branch list again. One held stays while it is read, and stays if it cannot
    /// be read.
    pub fn reload_branches(&mut self, cx: &mut Context<Self>) {
        self.read_branches(None, cx);
    }

    /// [`Self::reload_branches`] after Studio made or deleted `changed`: its head commit is
    /// read again when next asked for, whatever the lists show, since a branch
    /// deleted and made again under its name has another history.
    fn reload_branches_after(&mut self, changed: String, cx: &mut Context<Self>) {
        self.read_branches(Some(changed), cx);
    }

    fn read_branches(&mut self, changed: Option<String>, cx: &mut Context<Self>) {
        if !matches!(self.branches, Loadable::Loaded(_)) {
            self.branches = Loadable::Loading;
        }
        // A poll's list asked for before this answers for an older moment: it is not needed.
        self.branch_recheck = None;
        let client = self.client(cx);
        let graph = self.graph_id.clone();
        let moves = self.branch_moves;
        let sent = self.next_branch_list();
        let read = cx.background_spawn(async move { client.branches(&graph).await });
        self._branches_task = Some(cx.spawn(async move |this, cx| {
            let read = read.await;
            this.update(cx, |this, cx| this.note_read(&read, cx)).ok();
            let result = match read {
                Ok(output) => Loadable::Loaded(branch_order(output.branches)),
                Err(error) => Loadable::Failed(error.to_string()),
            };
            this.update(cx, |this, cx| {
                if let Some(changed) = &changed {
                    this.forget_branch(changed);
                }
                let result = match result {
                    // Asked for before a list already taken (a poll's, sent while this one was
                    // out and answered first): the newer stays, and nothing is dropped for a
                    // branch only it holds, nor kept for one it lacks.
                    Loadable::Loaded(_) if sent < this.branch_list_taken => return,
                    Loadable::Loaded(branches) => branches,
                    // The list held is still the graph's.
                    _ if matches!(this.branches, Loadable::Loaded(_)) => return,
                    failed => {
                        this.branches = failed;
                        cx.emit(SessionEvent::BranchesChanged);
                        cx.notify();
                        return;
                    }
                };
                this.branch_list_taken = sent;
                // A name the server lists again was made again since it was deleted.
                this.gone_branches.retain(|branch| !result.contains(branch));
                // The first list changes nothing read before it: lists asked for meanwhile
                // (main's, as the window opens) stay, rather than being read twice.
                if this.branches_read {
                    this.forget_branches_changed(&result, cx);
                }
                this.branches_read = true;
                this.branches = Loadable::Loaded(result);
                cx.emit(SessionEvent::BranchesChanged);
                cx.notify();
                this.leave_missing_branch(moves, cx);
            })
            .ok();
        }));
    }

    /// Forgets the head commit of every branch that came or went since the list
    /// held, or of all of them when none is held to compare with: a branch made, or deleted and
    /// made again under its name, has another history. A branch gone, however it was deleted
    /// (the CLI, an agent, a statement), takes its drafts.
    fn forget_branches_changed(&mut self, branches: &[String], cx: &mut Context<Self>) {
        let Some(held) = self.branches.loaded() else {
            self.head_commits.clear();
            return;
        };
        let gone: Vec<String> = held
            .iter()
            .filter(|branch| !branches.contains(branch))
            .cloned()
            .collect();
        let made: Vec<String> = branches
            .iter()
            .filter(|branch| !held.contains(branch))
            .cloned()
            .collect();
        for branch in gone.iter().chain(&made) {
            self.forget_branch(branch);
        }
        for branch in &gone {
            self.drop_drafts_of(branch, cx);
        }
    }

    /// Forgets what was read of `branch`'s history: its head commit.
    fn forget_branch(&mut self, branch: &String) {
        self.head_commits.forget(branch);
    }

    /// Moves the session to main when the branch list, asked for after its last move (`moves`),
    /// does not hold its branch: every read of a branch that is gone fails.
    fn leave_missing_branch(&mut self, moves: u64, cx: &mut Context<Self>) {
        let Loadable::Loaded(branches) = &self.branches else {
            return;
        };
        if moves != self.branch_moves || self.branch == "main" || branches.contains(&self.branch) {
            return;
        }
        let branch = self.branch.clone();
        self.set_branch("main".into(), cx);
        cx.emit(SessionEvent::BranchMissing { branch });
    }

    fn reload_counts(&mut self, cx: &mut Context<Self>) {
        self.datasets = Loadable::Loading;
        self.clear_counts();
        self.read_counts(cx);
    }

    /// Lets go of the type counts, with the snapshot version they came from and their commit,
    /// and of a read of that commit under way: it would name the commit of counts not held.
    fn clear_counts(&mut self) {
        self.dataset_counts.clear();
        self.counts_version = None;
        self.counts_commit = None;
        self._counts_commit_task = None;
    }

    /// Reads the counts again after a commit on the session's branch: the ones shown stay until
    /// the new ones are in, rather than blanking every count at each commit, and stay if they
    /// cannot be read: a failed read says nothing about them.
    /// A burst of commits (an agent's writes) reads them once more after the read under way,
    /// rather than dropping it and sending another at each.
    fn refresh_counts(&mut self, cx: &mut Context<Self>) {
        if self.counts_reading {
            self.counts_again = true;
        } else if matches!(self.datasets, Loadable::Loaded(_)) {
            self.read_counts(cx);
        } else {
            self.reload_counts(cx);
        }
    }

    fn read_counts(&mut self, cx: &mut Context<Self>) {
        self.counts_reading = true;
        self.counts_again = false;
        let client = self.client(cx);
        let graph = self.graph_id.clone();
        let branch = self.branch.clone();
        let read = {
            let branch = branch.clone();
            cx.background_spawn(async move { client.snapshot(&graph, &branch).await })
        };
        self._datasets_task = Some(cx.spawn(async move |this, cx| {
            let read = read.await;
            this.update(cx, |this, cx| this.note_read(&read, cx)).ok();
            this.update(cx, |this, cx| {
                this.counts_reading = false;
                match read {
                    Ok(output) => {
                        this.dataset_counts = output
                            .datasets
                            .iter()
                            .map(|dataset| {
                                let key = (dataset.entity_kind, dataset.type_name.clone());
                                (key, dataset.entity_count)
                            })
                            .collect();
                        this.datasets = Loadable::Loaded(output.datasets);
                        this.find_counts_commit(branch, output.graph_manifest_version, cx);
                        cx.emit(SessionEvent::CountsChanged);
                        cx.notify();
                    }
                    // A refresh: the counts held are the branch's until new ones are read.
                    Err(_) if matches!(this.datasets, Loadable::Loaded(_)) => {}
                    Err(error) => {
                        this.clear_counts();
                        this.datasets = Loadable::Failed(error.to_string());
                        cx.emit(SessionEvent::CountsChanged);
                        cx.notify();
                    }
                }
                if this.counts_again {
                    this.refresh_counts(cx);
                }
            })
            .ok();
        }));
    }

    /// The counts just taken came from the snapshot of `branch` at manifest `version`: finds
    /// the commit they are for. A snapshot names no commit, but each commit names the
    /// `__manifest` version that holds it, and one version of a branch's manifest holds one
    /// commit. So the branch's newest commit is read ([`Self::head_commit`], shared with every
    /// view that read it already), and when it is at `version` it is the counts' commit. A
    /// commit that landed between the two reads, or a manifest that moved without a commit,
    /// leaves the versions apart, and the counts are for no commit known
    /// ([`Self::counts_commit`]). A snapshot at the version held is of the same commit, which
    /// stays.
    fn find_counts_commit(&mut self, branch: String, version: u64, cx: &mut Context<Self>) {
        if self.counts_version != Some(version) {
            self.counts_commit = None;
        }
        self.counts_version = Some(version);
        if self.counts_commit.is_some() {
            return;
        }
        let read = self.head_commit(&branch, false, cx);
        self._counts_commit_task = Some(cx.spawn(async move |this, cx| {
            let Ok(Some(commit)) = read.await else {
                return;
            };
            this.update(cx, |this, _| {
                // The counts held since may be another snapshot's, or another branch's.
                if this.branch == branch
                    && this.counts_version == Some(version)
                    && commit.graph_manifest_version == version
                {
                    this.counts_commit = Some(commit.graph_commit_id);
                }
                this._counts_commit_task = None;
            })
            .ok();
        }));
    }

    /// This graph's one key for what Studio saves of it: its CLI target's name and its id,
    /// `<target>/<graph>` ([`studio_settings::overrides_key`]). Its settings (spec 002), its tabs
    /// and drafts, its saved queries' folder and its agent conversations are filed under it.
    pub fn overrides_key(&self, cx: &App) -> String {
        studio_settings::overrides_key(&self.connection.read(cx).server.name, &self.graph_id)
    }

    /// The browsing limits in effect for this graph: its overrides in the settings, else the
    /// shared ones.
    pub fn options(&self, cx: &App) -> studio_settings::GraphOptions {
        let target = &self.connection.read(cx).server.name;
        match cx.try_global::<studio_settings::StudioSettings>() {
            Some(settings) => settings.for_graph(target, &self.graph_id),
            None => studio_settings::StudioSettings::default().for_graph(target, &self.graph_id),
        }
    }

    /// The row count of a node or edge type at the branch head, from the snapshot.
    pub fn count_of(&self, kind: EntityKind, type_name: &str) -> Option<i64> {
        self.dataset_counts
            .get(&(kind, type_name.to_owned()))
            .copied()
    }

    /// The commit the counts of [`Self::count_of`] are for, once known: the branch's newest
    /// commit read after the snapshot, when it is at the snapshot's manifest version. `None`
    /// while it is read, and when it is at another version (a commit landed between the two
    /// reads, or the manifest moved without one): the counts are then for no commit known.
    pub fn counts_commit(&self) -> Option<&str> {
        self.counts_commit.as_deref()
    }

    pub fn set_branch(&mut self, branch: String, cx: &mut Context<Self>) {
        if self.branch == branch {
            return;
        }
        self.branch = branch;
        self.branch_moves += 1;
        // Not yet shown: the first show reads the graph on this branch.
        if !self.opened {
            cx.emit(SessionEvent::BranchChanged);
            cx.notify();
            return;
        }
        self.reload_counts(cx);
        cx.emit(SessionEvent::BranchChanged);
        cx.notify();
        self.check_heads(cx);
        let branch = AccessScope::branch(&self.branch);
        self.read_access(branch, cx);
    }

    /// The unsaved value of one property on the session's branch.
    pub fn draft(&self, key: &DraftKey) -> Option<&DraftEntry> {
        self.draft_on(&self.branch, key)
    }

    /// Every unsaved value on the session's branch, by type, id and property.
    pub fn drafts(&self) -> impl Iterator<Item = (&DraftKey, &DraftEntry)> {
        self.drafts_on(&self.branch)
    }

    /// Every unsaved value on `branch`.
    pub fn drafts_on(&self, branch: &str) -> impl Iterator<Item = (&DraftKey, &DraftEntry)> {
        self.drafts.get(branch).into_iter().flatten()
    }

    /// The unsaved value of one property on `branch`.
    pub fn draft_on(&self, branch: &str, key: &DraftKey) -> Option<&DraftEntry> {
        self.drafts.get(branch)?.get(key)
    }

    /// Whether a property's draft on `branch` is still `sent`, what the session held when a
    /// write or read went out with it: bound to the same value, with the same text, or still
    /// none.
    pub fn draft_as_sent(&self, branch: &str, key: &DraftKey, sent: Option<&DraftEntry>) -> bool {
        match (self.draft_on(branch, key), sent) {
            (None, None) => true,
            (Some(held), Some(sent)) => same_value(&held.old, &sent.old) && held.text == sent.text,
            _ => false,
        }
    }

    /// Settles a property's draft on `branch` once the write or read it waited on is in: `keep`
    /// replaces it, or `None` lets it go, unless the draft is no longer `sent`
    /// ([`Self::draft_as_sent`]). A draft retyped, rebound or let go elsewhere meanwhile (in a
    /// table cell, on a node's page) stands, and a branch deleted meanwhile keeps none: one made
    /// again under its name would show it, and saving it could write over a value that only
    /// reads the same. Whether it was settled.
    pub fn settle_draft_on(
        &mut self,
        branch: &str,
        key: DraftKey,
        sent: Option<&DraftEntry>,
        keep: Option<DraftEntry>,
        cx: &mut Context<Self>,
    ) -> bool {
        let as_sent = self.draft_as_sent(branch, &key, sent);
        if as_sent {
            match keep {
                Some(_) if self.gone_branches.contains(branch) => {}
                Some(entry) => self.set_draft_on(branch, key, entry, cx),
                None => {
                    self.remove_draft_on(branch, &key, cx);
                }
            }
        }
        as_sent
    }

    /// Keeps a property's unsaved value on the session's branch, replacing the one there.
    pub fn set_draft(&mut self, key: DraftKey, entry: DraftEntry, cx: &mut Context<Self>) {
        let branch = self.branch.clone();
        self.set_draft_on(&branch, key, entry, cx);
    }

    /// As [`GraphSession::set_draft`], on `branch`: an edit whose check comes back after the
    /// session moved stays on the branch it was typed on.
    pub fn set_draft_on(
        &mut self,
        branch: &str,
        key: DraftKey,
        entry: DraftEntry,
        cx: &mut Context<Self>,
    ) {
        self.gone_branches.remove(branch);
        let drafts = self.drafts.entry(branch.to_owned()).or_default();
        if drafts.get(&key) != Some(&entry) {
            drafts.insert(key, entry);
            cx.emit(SessionEvent::DraftsChanged);
            cx.notify();
        }
    }

    /// Lets a property's unsaved value go: saved, discarded or put back.
    pub fn remove_draft(&mut self, key: &DraftKey, cx: &mut Context<Self>) -> Option<DraftEntry> {
        let branch = self.branch.clone();
        self.remove_draft_on(&branch, key, cx)
    }

    /// As [`GraphSession::remove_draft`], on `branch`: a save that lands after the session moved
    /// to another branch lets go of the draft it saved, not that branch's.
    pub fn remove_draft_on(
        &mut self,
        branch: &str,
        key: &DraftKey,
        cx: &mut Context<Self>,
    ) -> Option<DraftEntry> {
        let removed = self.drafts.get_mut(branch)?.remove(key);
        if removed.is_some() {
            cx.emit(SessionEvent::DraftsChanged);
            cx.notify();
        }
        removed
    }

    /// Lets go of every unsaved value on `branch`, which was deleted: a branch made again under
    /// its name starts without them, and saving one could write it over a value that only
    /// reads the same.
    fn drop_drafts_of(&mut self, branch: &str, cx: &mut Context<Self>) {
        if self.gone_branches.len() >= GONE_BRANCHES {
            self.gone_branches.clear();
        }
        self.gone_branches.insert(branch.to_owned());
        if self
            .drafts
            .remove(branch)
            .is_some_and(|drafts| !drafts.is_empty())
        {
            cx.emit(SessionEvent::DraftsChanged);
            cx.notify();
        }
    }

    /// The commit `branch`'s head was last seen at, once a watched branch has been asked after
    /// or Studio committed to it.
    pub fn head(&self, branch: &str) -> Option<&str> {
        self.heads.get(branch).map(String::as_str)
    }

    /// Asks after `branch`'s head while the returned watch is held: a branch not watched yet
    /// at once, alone; the others at the next poll.
    pub fn watch_head(&mut self, branch: &str, cx: &mut Context<Self>) -> HeadWatch {
        let alive = Rc::new(());
        let new = !self
            .watched_branches()
            .iter()
            .any(|watched| watched == branch);
        self.head_watches
            .push((branch.to_owned(), Rc::downgrade(&alive)));
        // A parked graph asks after no head: shown, it asks after every watched one.
        if new && self.shown && self.unavailable.is_none() && !self.head_reads.contains_key(branch)
        {
            self.read_head(branch.to_owned(), cx);
        } else if new && !self.shown {
            self.head_check_missed = true;
        }
        HeadWatch { _alive: alive }
    }

    /// A scope for a view of `branch`: the view reads what it shows through it
    /// ([`GraphSession::read_in`]) while it holds it.
    pub fn read_scope(&mut self, branch: &str) -> ReadScope {
        self.next_scope += 1;
        let (scope, handle) = Scope::new(self.next_scope, branch.to_owned());
        self.scopes.retain(|_, scope| scope.is_alive());
        self.scopes.insert(self.next_scope, scope);
        handle
    }

    /// The view shows none of what it read through `scope` any more, and now reads `branch`.
    pub fn reset_scope(&mut self, scope: &ReadScope, branch: &str) {
        if let Some(scope) = self.scopes.get_mut(&scope.id) {
            scope.reset(branch.to_owned());
        }
    }

    /// Whether `scope`'s branch, moving from the commit the view shows (`from`) to `to`, changed
    /// anything the view read through it.
    pub fn scope_since(&self, scope: &ReadScope, from: &str, to: &str) -> ScopeSince {
        let Some(scope) = self.scopes.get(&scope.id) else {
            return ScopeSince::Changed;
        };
        if from == to {
            return ScopeSince::Unchanged;
        }
        match self.changes_since(&scope.branch, from, to) {
            Since::Pending => ScopeSince::Pending,
            Since::Known(since) if scope.unchanged(&since) => ScopeSince::Unchanged,
            Since::Known(_) | Since::Unknown => ScopeSince::Changed,
        }
    }

    /// [`GraphSession::read`] for what a view shows: `scope` keeps what the answer depends on
    /// from the moment it is asked.
    pub fn read_in(
        &mut self,
        scope: &ReadScope,
        query: &BuiltQuery,
        snapshot: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<ReadOutput, ReadError>> {
        let id = scope.id;
        let entry = self
            .scopes
            .get_mut(&id)
            .map(|scope| scope.add(query.depends.clone()));
        let read = self.read(query, snapshot, cx);
        // The token goes with this task: dropped by the view, the read stops counting as out.
        cx.spawn(async move |this, cx| {
            let result = read.await;
            if let Some((entry, _token)) = entry {
                this.update(cx, |this, _| {
                    if let Some(scope) = this.scopes.get_mut(&id) {
                        match &result {
                            Ok(output) => scope.answered(entry, &output.rows),
                            Err(_) => scope.failed(entry),
                        }
                    }
                })
                .ok();
            }
            result
        })
    }

    /// Reads `query` through `scope` while the view follows the branch ([`Self::read_in`]),
    /// else as it is.
    pub fn read_scoped(
        &mut self,
        scope: Option<&ReadScope>,
        query: &BuiltQuery,
        snapshot: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<ReadOutput, ReadError>> {
        match scope {
            Some(scope) => self.read_in(scope, query, snapshot, cx),
            None => self.read(query, snapshot, cx),
        }
    }

    /// Adds to what `scope`'s view shows an answer read outside it that depends on `depends`,
    /// with the nodes it lists when known (`None` counts the whole listed type).
    pub fn note_in(&mut self, scope: &ReadScope, depends: Depends, listed: Option<Vec<String>>) {
        if let Some(scope) = self.scopes.get_mut(&scope.id) {
            scope.add_known(depends, listed);
        }
    }

    /// [`GraphSession::degree_at`] for what a view shows: the count depends on the edges of the
    /// group's type at the node.
    pub(crate) fn degree_in(
        &mut self,
        scope: &ReadScope,
        type_name: &str,
        group: &RelationshipGroup,
        id: &str,
        snapshot: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<(u64, Option<String>), String>> {
        let scope_id = scope.id;
        let entry = self.scopes.get_mut(&scope_id).map(|scope| {
            scope.add(Depends::EdgesAt {
                edge: group.edge.clone(),
                nodes: vec![id.to_owned()],
                listed: None,
            })
        });
        let count = self.degree_at(type_name, group, id, snapshot, cx);
        let Some((entry, token)) = entry else {
            return count;
        };
        // Out while the view waits, and shown once in: a count its view stopped waiting for
        // (a read ahead for a row the pointer left) shows nothing.
        cx.spawn(async move |this, cx| {
            let _token = token;
            let result = count.await;
            this.update(cx, |this, _| {
                if let Some(scope) = this.scopes.get_mut(&scope_id) {
                    match &result {
                        Ok(_) => scope.answered(entry, &Value::Null),
                        Err(_) => scope.failed(entry),
                    }
                }
            })
            .ok();
            result
        })
    }

    /// [`GraphSession::blob_meta`] for what a view shows: the cell depends on its node or edge.
    pub fn blob_meta_in(
        &mut self,
        scope: &ReadScope,
        cell: BlobCell,
        snapshot: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<BlobMeta, ReadError>> {
        // A cell lists no nodes: it counts from the moment it is asked, answered or not.
        if let Some(scope) = self.scopes.get_mut(&scope.id) {
            let depends = if cell.edge {
                Depends::Types {
                    nodes: Vec::new(),
                    edges: vec![cell.type_name.clone()],
                }
            } else {
                Depends::Node {
                    type_name: cell.type_name.clone(),
                    id: cell.id.clone(),
                }
            };
            scope.add_known(depends, Some(Vec::new()));
        }
        self.blob_meta(cell, snapshot, cx)
    }

    /// What `branch`'s commits after `from`, up to and including `to`, changed among the types
    /// the change feed was asked about: [`Since::Pending`] while the feed is being read.
    pub fn changes_since(&self, branch: &str, from: &str, to: &str) -> Since {
        if let Some(since) = self
            .change_logs
            .get(branch)
            .and_then(|log| log.since(from, to))
        {
            return Since::Known(since);
        }
        if self.change_reads.contains_key(branch) {
            Since::Pending
        } else {
            Since::Unknown
        }
    }

    /// The type names what views on `branch` show depends on.
    fn read_names(&mut self, branch: &str) -> BTreeSet<String> {
        self.scopes.retain(|_, scope| scope.is_alive());
        self.scopes
            .values()
            .filter(|scope| scope.branch == branch)
            .flat_map(Scope::names)
            .collect()
    }

    /// Reads `branch`'s change feed after the newest commit its log holds, up to the feed's
    /// head, asked only about the types views read: every change carries its images, so an
    /// unfiltered read costs what every type wrote. The schema is compared afterwards unless
    /// every commit read is one of Studio's own data commits on top of the head held
    /// (`own_heads`), which leave it as it was, and, when the read could not tell every commit
    /// it skipped, no other move came since the last read (`foreign_moves`).
    fn read_changes(&mut self, branch: &str, cx: &mut Context<Self>) {
        if self.change_reads.contains_key(branch) {
            self.change_reads_again.insert(branch.to_owned());
            return;
        }
        // What this read answers for: a move after it starts is the next read's.
        let own: HashSet<String> = self.own_heads.get(branch).cloned().unwrap_or_default();
        let foreign = self.foreign_moves.remove(branch);
        let Some(head) = self.heads.get(branch).cloned() else {
            return;
        };
        let names = self.read_names(branch);
        let schema = self.schema_model().map(|schema| schema.source.clone());
        let log = self.change_logs.entry(branch.to_owned()).or_default();
        if log.holds(&head) {
            return;
        }
        let (Some(from), Some(schema)) = (log.last.clone(), schema) else {
            log.last = Some(head);
            return;
        };
        if names.is_empty() {
            // Nobody reads the branch: the log starts again at its head. Another writer's
            // commit may have changed the schema, which a query editor or History open on the
            // graph reads under: it is compared, as the feed read would. Studio's own data
            // commits change no schema.
            log.last = Some(head);
            self.own_heads.remove(branch);
            if foreign {
                self.recheck_schema(None, cx);
            }
            return;
        }
        let (client, graph, epoch) = (self.client(cx), self.graph_id.clone(), self.schema_epoch);
        let types: Vec<String> = names.iter().cloned().collect();
        let mut fold = FeedFold::new(Arc::new(names));
        let key = branch.to_owned();
        let branch = branch.to_owned();
        let answered = own.clone();
        // The feed's pages are read and folded off the main thread: each change carries its
        // images, and a page can be large.
        let feed_branch = branch.clone();
        let fed = cx.background_spawn(async move {
            let branch = feed_branch;
            let mut next = Some(FeedNext::Start(format!("after:{from}")));
            let mut failed = None;
            for _ in 0..FEED_PAGES {
                let Some(at) = next.take() else {
                    break;
                };
                match client
                    .changes(&graph, &branch, at.at(), &types, FEED_PAGE)
                    .await
                {
                    Ok(page) => {
                        fold.page(&page);
                        next = match (page.next_page_token, page.cursor, page.caught_up) {
                            (Some(token), ..) => Some(FeedNext::Page(token)),
                            (None, _, Some(true)) | (None, None, _) => None,
                            // More complete commits wait past the page's limit.
                            (None, Some(cursor), _) => Some(FeedNext::Cursor(cursor)),
                        };
                    }
                    // A schema boundary (409), reclaimed history (410) or a failed read: the
                    // commits read before it stand.
                    Err(error) => {
                        failed = Some(error);
                        break;
                    }
                }
            }
            let commits = fold.complete();
            // The feed filters by type name, so a schema change in between (a rename) could
            // hide a view's changes: what it says counts only under the schema views read with.
            // Studio's own commits on top of the heads held changed none; a read that stopped
            // short skips commits it never saw, which another writer's move may be among.
            let all_own = commits.iter().all(|(commit, _)| own.contains(commit));
            let stopped_short = failed.is_some() || next.is_some();
            let schema_read = if all_own && !(stopped_short && foreign) {
                None
            } else {
                Some(client.schema(&graph).await)
            };
            (commits, failed, next, schema_read)
        });
        let read = cx.spawn(async move |this, cx| {
            let (commits, failed, next, schema_read) = fed.await;
            this.update(cx, |this, cx| {
                this.change_reads.remove(&branch);
                if let Some(error) = failed.clone() {
                    this.note_read::<()>(&Err(error), cx);
                }
                let same_schema = this.schema_epoch == epoch
                    && match &schema_read {
                        None => true,
                        Some(Ok(output)) => *output.schema_source == *schema,
                        Some(Err(_)) => false,
                    };
                // Another writer changed the schema: a feed that crossed it refuses (409), or
                // the schema read after it says so.
                // A schema read that failed leaves it unknown, and it is compared again.
                let schema_moved = matches!(&failed, Some(ReadError::Refused(refusal)) if refusal.status == 409)
                    || matches!(&schema_read, Some(Ok(output)) if *output.schema_source != *schema)
                    || matches!(&schema_read, Some(Err(_)));
                let stopped = failed.is_some() || next.is_some() || !same_schema;
                // Studio's own commits this read answered for are past: read, or skipped by a
                // read that stopped short.
                if let Some(held) = this.own_heads.get_mut(&branch) {
                    held.retain(|commit| !answered.contains(commit));
                    if held.is_empty() {
                        this.own_heads.remove(&branch);
                    }
                }
                let head = this.heads.get(&branch).cloned();
                let log = this.change_logs.entry(branch.clone()).or_default();
                if same_schema {
                    for (commit, logged) in commits {
                        log.record(commit, logged);
                    }
                }
                // The commits a read stopped short of stay unknown; the next read starts at the
                // head. That head can be past another writer's move made while this read was
                // out, which the next read then never sees: the schema is compared now.
                let mut jumped = false;
                if let Some(head) = head
                    && stopped
                    && !log.holds(&head)
                {
                    log.last = Some(head);
                    jumped = true;
                }
                let moved_meanwhile = jumped && this.foreign_moves.remove(&branch);
                cx.emit(SessionEvent::ChangesRead {
                    branch: branch.clone(),
                });
                // A schema that changed again while this read was out started a new epoch,
                // which drops this read's text: one newer than the epoch's is read again.
                let newer_text = this.schema_epoch != epoch
                    && matches!(&schema_read, Some(Ok(output))
                        if this.schema_text.as_deref() != Some(&*output.schema_source));
                if moved_meanwhile || newer_text {
                    this.recheck_schema(None, cx);
                }
                if schema_moved && this.schema_epoch == epoch {
                    // The text just read, when there is one, is not read again.
                    let read = match schema_read {
                        Some(Ok(output)) => Some(output),
                        _ => None,
                    };
                    this.recheck_schema(read, cx);
                }
                if this.change_reads_again.remove(&branch) {
                    this.read_changes(&branch, cx);
                }
            })
            .ok();
        });
        self.change_reads.insert(key, read);
    }

    /// A window became active: asks after the heads at once when a poll came while none was,
    /// unless no window shows the graph.
    pub fn window_activated(&mut self, cx: &mut Context<Self>) {
        if self.shown && std::mem::take(&mut self.head_check_missed) {
            self.check_heads(cx);
        }
    }

    /// Whether a window shows the graph. A graph its window parked (another graph shown in its
    /// place) asks after no head and no branch list, stops holding its server's health checks
    /// ([`Connection::watch`]), and reads nothing again for a new token, a 403 or a 503; shown
    /// again, it reads what those owe, and asks after its heads at once when a poll came
    /// meanwhile, as a window's activation does. A session opened behind ([`Self::parked`])
    /// sends its first reads when first shown; one made with [`Self::new`] is shown already.
    /// Either sends them only once its connection's token is ready ([`Self::open_when_ready`]).
    pub fn set_shown(&mut self, shown: bool, cx: &mut Context<Self>) {
        if self.shown == shown {
            return;
        }
        self.shown = shown;
        if !shown {
            self._connection_watch = None;
            return;
        }
        self._connection_watch = Some(self.connection.update(cx, |c, cx| c.watch(cx)));
        if !self.opened {
            self.open_when_ready(cx);
            return;
        }
        // Its nodes named otherwise while parked: views read under the new names now.
        if std::mem::take(&mut self.names_owed) {
            cx.emit(SessionEvent::SchemaChanged);
            cx.notify();
        }
        // A new token, or a 403 on the graph, while parked: what the graph shows, and what the
        // credential may do, are read now.
        if std::mem::take(&mut self.credential_owed) {
            self.read_again_for_credential(cx);
        }
        if std::mem::take(&mut self.access_owed) {
            self.access_changed(cx);
        }
        // The graph answered 503, and its wait ended while parked.
        if std::mem::take(&mut self.recheck_owed) {
            self.recheck_available(cx);
        }
        // A view wanted the catalog while parked.
        if self.stored_queries_wanted && matches!(self.stored_queries, Loadable::NotLoaded) {
            self.read_stored_queries(false, cx);
        }
        if cx.active_window().is_some() && std::mem::take(&mut self.head_check_missed) {
            self.check_heads(cx);
        }
    }

    /// Sends the first reads of a graph a window shows, once its connection's token is ready
    /// ([`Connection::token_ready`]): the CLI gives a server's token some tens or hundreds of
    /// milliseconds after the connection asks, and a read sent before carries none, so the
    /// server refuses it (401). Until then the window waits for the token, which the CLI serves
    /// first ([`Connection::wait_for_token`]), and [`ConnectionEvent::TokenReady`] brings the
    /// session here again.
    fn open_when_ready(&mut self, cx: &mut Context<Self>) {
        if self.opened || !self.shown {
            return;
        }
        if !self.connection.read(cx).token_ready() {
            self.connection
                .update(cx, |connection, cx| connection.wait_for_token(cx));
            return;
        }
        self.opened = true;
        // The first reads cover whatever a token, a refusal or a poll owed meanwhile, and the
        // schema's answer asks after the heads.
        self.credential_owed = false;
        self.access_owed = false;
        self.recheck_owed = false;
        self.names_owed = false;
        self.head_check_missed = false;
        self.credential = self.client(cx).credential_changes();
        self.reload(cx);
        let branch = AccessScope::branch(&self.branch);
        self.read_access(branch, cx);
    }

    /// Whether a window shows the graph ([`Self::set_shown`]).
    pub fn is_shown(&self) -> bool {
        self.shown
    }

    /// The session's branch, then every branch a held watch names.
    fn watched_branches(&self) -> Vec<String> {
        let mut branches = vec![self.branch.clone()];
        for (branch, alive) in &self.head_watches {
            if alive.strong_count() > 0 && !branches.contains(branch) {
                branches.push(branch.clone());
            }
        }
        branches
    }

    /// Reads each watched branch's head with a read that returns no rows, unless one is under
    /// way, and the branch list. Heads of branches no longer watched are forgotten. A parked
    /// graph asks nothing: it asks when shown again.
    pub fn check_heads(&mut self, cx: &mut Context<Self>) {
        if !self.shown {
            self.head_check_missed = true;
            return;
        }
        self.head_watches
            .retain(|(_, alive)| alive.strong_count() > 0);
        let watched = self.watched_branches();
        self.heads.retain(|branch, _| watched.contains(branch));
        self.placing.retain(|branch, _| watched.contains(branch));
        self.own_heads.retain(|branch, _| watched.contains(branch));
        self.foreign_moves.retain(|branch| watched.contains(branch));
        self.change_logs
            .retain(|branch, _| watched.contains(branch));
        for branch in std::mem::replace(&mut self.watched, watched.clone()) {
            if !watched.contains(&branch) {
                self.forget_branch(&branch);
            }
        }
        if self.unavailable.is_some() {
            return;
        }
        self.recheck_branches(cx);
        for branch in watched {
            if !self.head_reads.contains_key(&branch) {
                self.read_head(branch, cx);
            }
        }
    }

    /// Reads `branch`'s head now, and resolves once the session holds what it answered, or no
    /// read could be sent: a Refresh reads at the newest head, which the next poll would only
    /// find later, and a view pinned at it still compares the heads after it.
    pub fn head_now(&mut self, branch: &str, cx: &mut Context<Self>) -> Task<()> {
        self.read_head(branch.to_owned(), cx);
        if !self.head_reads.contains_key(branch) {
            return Task::ready(());
        }
        let (landed, waited) = oneshot::channel();
        self.head_waiters
            .entry(branch.to_owned())
            .or_default()
            .push(landed);
        cx.background_spawn(async move {
            waited.await.ok();
        })
    }

    /// The head read of `branch` landed, or none could be sent.
    fn head_landed(&mut self, branch: &str) {
        for waiter in self.head_waiters.remove(branch).into_iter().flatten() {
            waiter.send(()).ok();
        }
    }

    /// Reads `branch`'s head with a read that returns no rows. What waits on a head read
    /// ([`Self::head_now`]) is released when it lands, or at once when none can be sent.
    fn read_head(&mut self, branch: String, cx: &mut Context<Self>) {
        let Some(schema) = self.schema_model() else {
            self.head_landed(&branch);
            return;
        };
        let Some(probe) = builder::head_probe(&schema) else {
            // A schema with no node type leaves the probe nothing to match: the schema is read
            // instead, which is how a first type applied elsewhere is seen.
            self.recheck_schema(None, cx);
            self.head_landed(&branch);
            return;
        };
        let request = QueryRequest {
            query: probe.text,
            name: Some(probe.name),
            params: None,
            branch: Some(branch.clone()),
            snapshot: None,
        };
        let client = self.client(cx);
        let graph = self.graph_id.clone();
        let key = branch.clone();
        let probe = cx.background_spawn(async move { client.query(&graph, &request).await });
        let read = cx.spawn(async move |this, cx| {
            let read = probe.await;
            this.update(cx, |this, cx| {
                this.head_reads.remove(&branch);
                this.note_read(&read, cx);
                match read {
                    Ok(ReadOutput {
                        graph_commit_id: Some(head),
                        ..
                    }) => this.saw_head(branch.clone(), head, false, None, cx),
                    // The probe matches a type of the schema held, which another writer renamed
                    // or dropped: the schema is read again, and the heads under it.
                    Err(ReadError::Refused(refusal)) if refusal.status == 400 => {
                        this.recheck_schema(None, cx)
                    }
                    _ => {}
                }
                this.head_landed(&branch);
            })
            .ok();
        });
        self.head_reads.insert(key, read);
    }

    /// A number for a branch-list read about to be sent: the lists are taken in the order
    /// they were asked for, whatever order they answer in.
    fn next_branch_list(&mut self) -> u64 {
        self.branch_lists_sent += 1;
        self.branch_lists_sent
    }

    /// Reads the branch list again once it has been read, so a branch made or deleted elsewhere
    /// (the CLI, an agent, another window) appears; an unchanged list changes nothing.
    fn recheck_branches(&mut self, cx: &mut Context<Self>) {
        if self.branch_recheck.is_some() || !matches!(self.branches, Loadable::Loaded(_)) {
            return;
        }
        let client = self.client(cx);
        let graph = self.graph_id.clone();
        let moves = self.branch_moves;
        let sent = self.next_branch_list();
        let read = cx.background_spawn(async move { client.branches(&graph).await });
        self.branch_recheck = Some(cx.spawn(async move |this, cx| {
            let read = read.await;
            this.update(cx, |this, cx| {
                this.branch_recheck = None;
                this.note_read(&read, cx);
                let Ok(output) = read else {
                    return;
                };
                // A list read again since, and taken, is newer.
                if sent < this.branch_list_taken {
                    return;
                }
                this.branch_list_taken = sent;
                let branches = branch_order(output.branches);
                if this.branches.loaded() != Some(&branches) {
                    this.forget_branches_changed(&branches, cx);
                    this.branches = Loadable::Loaded(branches);
                    cx.emit(SessionEvent::BranchesChanged);
                    cx.notify();
                }
                this.leave_missing_branch(moves, cx);
            })
            .ok();
        }));
    }

    /// Records `branch`'s head. A move from a head already seen that is not one of Studio's own
    /// commits (`own`) is another writer's commit: the branch's head commit and counts are read
    /// again, as after Studio's own.
    fn saw_head(
        &mut self,
        branch: String,
        head: String,
        own: bool,
        parent: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        // A head read sent before this commit could answer with the head before it: it is sent
        // again once the commit is taken, so what waits on it hears a head no older.
        let reading = own && self.head_reads.remove(&branch).is_some();
        // The head read placing one of Studio's own commits (below) found it at the head: the
        // commit was told already, and is no other writer's. Another of Studio's commits leaves
        // the placing to that read.
        let placed = !own
            && self
                .placing
                .remove(&branch)
                .is_some_and(|commit| commit == head);
        let previous = self.heads.get(&branch).cloned();
        if previous.as_deref() == Some(head.as_str()) {
            if reading {
                self.read_head(branch, cx);
            }
            return;
        }
        if own
            && previous.is_some()
            && parent != previous.as_deref()
            && self.watched_branches().contains(&branch)
        {
            // Not on top of the head held, which may be newer: a write that took minutes
            // answers after the watch saw another writer's commit on top of it, and an agent's
            // write names no parent. The head held stays, and is read again now rather than at
            // the next poll to say which is newer.
            self.placing.insert(branch.clone(), head);
            self.read_head(branch, cx);
            return;
        }
        self.heads.insert(branch.clone(), head.clone());
        // Before views hear of the move, so they find the read under way. Only Studio's own
        // commit on top of the head held leaves the schema as it was: one placed by a head read
        // sits on another writer's.
        if own && previous.is_some() && parent == previous.as_deref() {
            self.own_heads
                .entry(branch.clone())
                .or_default()
                .insert(head.clone());
        } else if previous.is_some() {
            self.foreign_moves.insert(branch.clone());
        }
        self.read_changes(&branch, cx);
        if reading {
            self.read_head(branch.clone(), cx);
        }
        if previous.is_some() && !own && !placed {
            self.forget_branch(&branch);
            if branch == self.branch {
                self.refresh_counts(cx);
            }
            cx.emit(SessionEvent::Committed {
                branch: branch.clone(),
            });
        }
        cx.emit(SessionEvent::HeadMoved { branch, head });
    }

    /// Keeps what an answer's rows also answer (`builder::Seeds`): whole nodes as their node
    /// reads, counts as each node's count. Nothing already held is replaced, and nothing is kept
    /// when the schema or the token changed while it was read (`epoch`, `credential`).
    /// `nodes` are the node reads a page's rows answer ([`node_answers`]), made off the main
    /// thread.
    fn seed(
        &mut self,
        seeds: &Seeds,
        output: &ReadOutput,
        nodes: Vec<(PinnedRead, ReadOutput)>,
        epoch: u64,
        credential: u64,
    ) {
        let Some(commit) = output.graph_commit_id.clone() else {
            return;
        };
        if epoch != self.schema_epoch || credential != self.credential {
            return;
        }
        match seeds {
            Seeds::Nodes { .. } => {
                for (key, answer) in nodes {
                    if !self.pinned_reads.holds(&key) && !self.seeded_reads.holds(&key) {
                        self.seeded_at(&key);
                        self.seeded_reads.insert(key, answer);
                    }
                }
            }
            Seeds::Degrees {
                type_name,
                group,
                ids,
            } => {
                let Some(counts) = page_counts(&output.rows) else {
                    return;
                };
                for id in ids {
                    let key = DegreeAt {
                        type_name: type_name.clone(),
                        group: group.clone(),
                        id: id.clone(),
                        commit: commit.clone(),
                        schema: epoch,
                    };
                    let n = counts.get(id).copied().unwrap_or(0);
                    self.count_kept_at(&key);
                    self.degree_counts.insert(key, n);
                }
            }
        }
    }

    /// A node's count through a group kept at `key`'s commit, or at an earlier commit of the
    /// session's branch when no edge of the group's type at the node changed in between.
    fn degree_kept(&mut self, key: &DegreeAt) -> Option<u64> {
        if let Some(Held::Done(n)) = self.degree_counts.get(key) {
            return Some(n);
        }
        let family = (
            key.type_name.clone(),
            key.group.clone(),
            key.id.clone(),
            key.schema,
        );
        let kept = self.degree_commits.get_mut(&family)?;
        let counts = &self.degree_counts;
        let at = |commit: &str| DegreeAt {
            commit: commit.to_owned(),
            ..key.clone()
        };
        let carried = carry_back(
            self.change_logs.get(&self.branch),
            &key.commit,
            kept,
            |commit| counts.peek(&at(commit)).is_some(),
            |start, since| {
                counts
                    .peek(&at(start))
                    .filter(|_| !since.edges_at(&key.group.edge, &key.id))
                    .copied()
            },
        )?;
        self.count_kept_at(key);
        self.degree_counts.insert(key.clone(), carried);
        Some(carried)
    }

    /// Notes that a count for `key` is kept, so a miss at another commit finds it.
    fn count_kept_at(&mut self, key: &DegreeAt) {
        let family = (
            key.type_name.clone(),
            key.group.clone(),
            key.id.clone(),
            key.schema,
        );
        remember(&mut self.degree_commits, family, &key.commit, DEGREE_COUNTS);
    }

    /// An answer to the same query and parameters read at an earlier commit of the session's
    /// branch, standing for `key`'s commit because no commit in between changed what it
    /// depends on (spec 015, carry-forward). Answered as read at `key`'s commit.
    fn carried(&mut self, key: &PinnedRead, depends: &Depends) -> Option<ReadOutput> {
        if *depends == Depends::Unknown {
            return None;
        }
        let family = (key.query.clone(), key.params.clone(), key.schema);
        let (reads, seeded) = (&self.pinned_reads, &self.seeded_reads);
        let log = self.change_logs.get(&self.branch);
        let at = |commit: &str| PinnedRead {
            commit: commit.to_owned(),
            ..key.clone()
        };
        let answer = |commit: &str| {
            let at = at(commit);
            reads.peek(&at).or_else(|| seeded.peek(&at))
        };
        // The commits it was read at, then those a page's rows gave it at.
        let mut output = [&mut self.pinned_commits, &mut self.seeded_commits]
            .into_iter()
            .find_map(|families| {
                carry_back(
                    log,
                    &key.commit,
                    families.get_mut(&family)?,
                    |commit| answer(commit).is_some(),
                    |start, since| {
                        answer(start)
                            .filter(|output| answer_unchanged(since, depends, &output.rows))
                            .cloned()
                    },
                )
            })?;
        output.graph_commit_id = Some(key.commit.clone());
        output.target.branch = None;
        output.target.snapshot = Some(key.commit.clone());
        Some(output)
    }

    /// The memo key of `query` read at `commit`, under the session's schema.
    fn pinned_key(&self, query: &BuiltQuery, commit: &str) -> PinnedRead {
        PinnedRead::of(query, commit, self.schema_epoch)
    }

    /// Notes that a page's row gave the answer for `key`, so a miss at another commit finds it.
    fn seeded_at(&mut self, key: &PinnedRead) {
        let family = (key.query.clone(), key.params.clone(), key.schema);
        remember(&mut self.seeded_commits, family, &key.commit, SEEDED_READS);
    }

    /// Notes that an answer for `key` is kept, so a miss at another commit finds it.
    fn kept_at(&mut self, key: &PinnedRead) {
        let family = (key.query.clone(), key.params.clone(), key.schema);
        remember(&mut self.pinned_commits, family, &key.commit, PINNED_READS);
    }

    /// Runs a read on the session's branch, or at `snapshot` when given (constitution II). A
    /// read at a commit is answered from what was read at that commit before, or shares an
    /// identical read under way: a commit never changes. What its rows also answer is kept
    /// once per answer read or carried ([`Self::seeding`]), not again for each view that
    /// shares it.
    pub fn read(
        &mut self,
        query: &BuiltQuery,
        snapshot: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<ReadOutput, ReadError>> {
        let request = QueryRequest {
            query: query.text.clone(),
            name: Some(query.name.clone()),
            params: query.params_value(),
            branch: snapshot.is_none().then(|| self.branch.clone()),
            snapshot: snapshot.clone(),
        };
        let Some(commit) = snapshot else {
            let read = self.read_request(request, cx);
            return self.seeding(query.seeds.clone(), read, cx);
        };
        let key = self.pinned_key(query, &commit);
        // A page's row answered it at this commit.
        if !self.pinned_reads.holds(&key)
            && let Some(Held::Done(output)) = self.seeded_reads.get(&key)
        {
            return self.seeding(query.seeds.clone(), Task::ready(Ok(output)), cx);
        }
        // Neither an answer nor a read someone still waits for: carried from an earlier
        // commit when nothing it depends on changed since.
        let none = match self.pinned_reads.held(&key) {
            None => true,
            Some(Held::Pending(pending)) => pending.read.upgrade().is_none(),
            Some(Held::Done(_)) => false,
        };
        if none && let Some(output) = self.carried(&key, &query.depends) {
            self.kept_at(&key);
            self.pinned_reads.insert(key, output.clone());
            return self.seeding(query.seeds.clone(), Task::ready(Ok(output)), cx);
        }
        // An answer kept filed what its rows answer when it was read; a read under way files
        // them as it ends, for everyone who shares it.
        let seeds = query.seeds.clone();
        self.share(
            |session| &mut session.pinned_reads,
            key,
            move |session, _, cx| {
                let read = session.read_at_commit(request, cx);
                session.seeding(seeds, read, cx)
            },
            Self::kept_at,
            cx,
        )
    }

    /// `read`, which on its answer also keeps what its rows answer (`seeds`, [`Self::seed`]),
    /// at the commit and under the schema it was read with.
    fn seeding(
        &mut self,
        seeds: Option<Seeds>,
        read: Task<Result<ReadOutput, ReadError>>,
        cx: &mut Context<Self>,
    ) -> Task<Result<ReadOutput, ReadError>> {
        let Some(seeds) = seeds else {
            return read;
        };
        let (epoch, credential) = (self.schema_epoch, self.credential);
        let schema = self.schema_model();
        cx.spawn(async move |this, cx| {
            let result = read.await;
            if let Ok(output) = &result {
                // A page holds hundreds of nodes, each a query to build and a row to copy.
                let nodes = match (&seeds, schema) {
                    (Seeds::Nodes { type_name }, Some(schema)) => {
                        let (type_name, output) = (type_name.clone(), output.clone());
                        cx.background_spawn(async move {
                            node_answers(&schema, &type_name, &output, epoch)
                        })
                        .await
                    }
                    _ => Vec::new(),
                };
                this.update(cx, |this, _| {
                    this.seed(&seeds, output, nodes, epoch, credential)
                })
                .ok();
            }
            result
        })
    }

    /// [`GraphSession::read`] of `branch`'s head, though the session moved to another branch
    /// meanwhile: a view bound to the branch it read on reads it again there.
    pub fn read_on(
        &mut self,
        branch: &str,
        query: &BuiltQuery,
        cx: &mut Context<Self>,
    ) -> Task<Result<ReadOutput, ReadError>> {
        if branch == self.branch {
            return self.read(query, None, cx);
        }
        let request = QueryRequest {
            query: query.text.clone(),
            name: Some(query.name.clone()),
            params: query.params_value(),
            branch: Some(branch.to_owned()),
            snapshot: None,
        };
        self.read_request(request, cx)
    }

    /// Names of the nodes `ends` lists, `(type, id)`, by their type's label property, read at
    /// `snapshot` (the session's branch when `None`): one read per type of at most
    /// [`NAME_BATCH`] ids, the first [`NAME_LIMIT`] ends only. A node whose label is its key, or
    /// that a read did not find, is left out, and so is every node of a read that failed: a
    /// name only replaces an id its caller already shows.
    pub fn node_names(
        &mut self,
        ends: Vec<(String, String)>,
        snapshot: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<crate::NodeNames> {
        let Some(schema) = self.schema_model() else {
            return Task::ready(crate::NodeNames::default());
        };
        let mut by_type: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (type_name, id) in ends.into_iter().take(NAME_LIMIT) {
            by_type.entry(type_name).or_default().push(id);
        }
        let mut reads = Vec::new();
        for (type_name, ids) in by_type {
            for chunk in ids.chunks(NAME_BATCH) {
                if let Ok(Some(query)) = builder::node_labels(&schema, &type_name, chunk) {
                    reads.push((type_name.clone(), self.read(&query, snapshot.clone(), cx)));
                }
            }
        }
        cx.spawn(async move |_, _| {
            let mut names = crate::NodeNames::default();
            for (type_name, read) in reads {
                let Ok(output) = read.await else { continue };
                for row in output.rows.as_array().into_iter().flatten() {
                    if let (Some(id), Some(label)) = (
                        row.get("id").and_then(Value::as_str),
                        row.get("label").and_then(Value::as_str),
                    ) && !label.is_empty()
                    {
                        names.insert((type_name.clone(), id.to_owned()), label.to_owned());
                    }
                }
            }
            names
        })
    }

    /// What `memo` keeps for `key`: its answer, the read under way that someone still waits
    /// for, or a new read `start` begins, given the ticket the memo files it under. Every
    /// asker's task files the answer under the read it shared, and `answered` runs when one is
    /// kept. Dropping every asker's task stops the read.
    fn share<K, V>(
        &mut self,
        memo: fn(&mut Self) -> &mut ReadMemo<K, V>,
        key: K,
        start: impl FnOnce(&mut Self, u64, &mut Context<Self>) -> Task<Result<V, ReadError>>,
        answered: fn(&mut Self, &K),
        cx: &mut Context<Self>,
    ) -> Task<Result<V, ReadError>>
    where
        K: std::hash::Hash + Eq + Clone + 'static,
        V: Clone + 'static,
    {
        let shared = match memo(self).get(&key) {
            Some(Held::Done(value)) => return Task::ready(Ok(value)),
            Some(Held::Pending(pending)) => {
                pending.read.upgrade().map(|read| (read, pending.ticket))
            }
            None => None,
        };
        let (read, ticket) = match shared {
            Some(shared) => shared,
            None => {
                self.next_ticket += 1;
                let ticket = self.next_ticket;
                let read = start(self, ticket, cx).shared();
                if let Some(weak) = read.downgrade() {
                    memo(self).start(key.clone(), Pending { read: weak, ticket });
                }
                (read, ticket)
            }
        };
        cx.spawn(async move |this, cx| {
            let result = read.await;
            this.update(cx, |this, _| {
                let kept = memo(this).finish(&key, &result, |pending| pending.ticket == ticket);
                if kept && result.is_ok() {
                    answered(this, &key);
                }
            })
            .ok();
            result
        })
    }

    /// A read at `request`'s commit. When that commit is the session branch's head, it is sent
    /// as a read of the branch, which the server answers from its warm caches where a read
    /// pinned to a commit resolves the commit and opens the graph at it, 0.5 s or more slower
    /// (spec 015). Its answer counts only if it comes back at that commit, so the read still
    /// sees exactly the commit asked for; one from a newer head is read again, pinned. Only the
    /// head watch and Studio's own commits move a head, since reads can finish out of order.
    fn read_at_commit(
        &mut self,
        request: QueryRequest,
        cx: &mut Context<Self>,
    ) -> Task<Result<ReadOutput, ReadError>> {
        let Some(commit) = request.snapshot.clone() else {
            return self.read_request(request, cx);
        };
        // A branch whose head is the commit answers the read faster than the commit does: the
        // session's own branch, else another watched one at it (a review's, an agent's).
        let at_head = if self.heads.get(&self.branch) == Some(&commit) {
            Some(self.branch.clone())
        } else {
            self.heads
                .iter()
                .find(|(_, head)| **head == commit)
                .map(|(branch, _)| branch.clone())
        };
        let Some(branch) = at_head else {
            return self.read_request(request, cx);
        };
        let at_branch = QueryRequest {
            branch: Some(branch),
            snapshot: None,
            ..request.clone()
        };
        let branch_read = self.read_request(at_branch, cx);
        cx.spawn(async move |this, cx| {
            match branch_read.await {
                Ok(mut output) if output.graph_commit_id.as_deref() == Some(commit.as_str()) => {
                    // Answered as the pinned read would be.
                    output.target.branch = None;
                    output.target.snapshot = Some(commit);
                    return Ok(output);
                }
                // The branch moved past the commit, or went (a branch the server does not
                // know is not found): read at the commit itself. Any other refusal is the
                // read's own, which the commit would answer alike: on this route a 409 is a
                // full-text index to rebuild, which no read clears.
                Ok(_) | Err(ReadError::Refused(ApiErrorSummary { status: 404, .. })) => {}
                Err(error) => return Err(error),
            }
            let pinned = this.update(cx, |this, cx| this.read_request(request, cx));
            match pinned {
                Ok(pinned) => pinned.await,
                Err(_) => Err(ReadError::Transport(
                    "Studio stopped before the server answered".into(),
                )),
            }
        })
    }

    pub fn read_request(
        &mut self,
        request: QueryRequest,
        cx: &mut Context<Self>,
    ) -> Task<Result<ReadOutput, ReadError>> {
        let client = self.client(cx);
        let graph = self.graph_id.clone();
        self.counted_read(async move { client.query(&graph, &request).await }, cx)
    }

    /// Runs a read counted in [`Self::reads_in_flight`] for exactly as long as it runs. A read
    /// nobody waits for any more (its task dropped, as a search does when the text changes)
    /// stops, and is counted out either way: a count that only came down on an answer would
    /// leave "queries running" up for reads that no longer exist.
    fn counted_read<T: Send + 'static>(
        &mut self,
        request: impl Future<Output = Result<T, ReadError>> + Send + 'static,
        cx: &mut Context<Self>,
    ) -> Task<Result<T, ReadError>> {
        self.reads_in_flight += 1;
        cx.emit(SessionEvent::ActivityChanged);
        let (answer_tx, answer_rx) = oneshot::channel();
        // Held by the caller's task: dropping that task drops this, which stops the read.
        let (waiting_tx, waiting_rx) = oneshot::channel::<std::convert::Infallible>();
        // The request, its body and its decoding run off the main thread, which only takes the
        // answer: a large page or feed decoded there would hold up frames.
        let request = cx.background_spawn(request);
        cx.spawn(async move |this, cx| {
            let result = match select(std::pin::pin!(request), waiting_rx).await {
                Either::Left((result, _)) => Some(result),
                Either::Right(_) => None,
            };
            this.update(cx, |this, cx| {
                if let Some(result) = &result {
                    this.note_read(result, cx);
                }
                this.reads_in_flight = this.reads_in_flight.saturating_sub(1);
                cx.emit(SessionEvent::ActivityChanged);
                cx.notify();
            })
            .ok();
            if let Some(result) = result {
                answer_tx.send(result).ok();
            }
        })
        .detach();
        cx.spawn(async move |_, _| {
            let _waiting = waiting_tx;
            answer_rx.await.unwrap_or_else(|_| {
                Err(ReadError::Transport(
                    "Studio stopped before the server answered".into(),
                ))
            })
        })
    }

    /// Runs a write to its outcome whether or not anyone still waits for it. Dropping the
    /// caller's task must not cut a mutation off mid-flight: its outcome would go unlogged
    /// and its count would never come down, and nothing may resend it.
    fn to_outcome<T: 'static>(
        work: Task<WriteOutcome<T>>,
        cx: &mut Context<Self>,
    ) -> Task<WriteOutcome<T>> {
        let (outcome_tx, outcome_rx) = oneshot::channel();
        cx.spawn(async move |_, _| {
            outcome_tx.send(work.await).ok();
        })
        .detach();
        cx.spawn(async move |_, _| {
            outcome_rx.await.unwrap_or_else(|_| WriteOutcome::Unknown {
                reason: "Studio stopped before the server answered".into(),
            })
        })
    }

    /// What a Blob cell holds (spec 004): its size, or the reference it names, or null. Read at
    /// `snapshot`, else the session's branch head.
    pub fn blob_meta(
        &mut self,
        cell: BlobCell,
        snapshot: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<BlobMeta, ReadError>> {
        // At a commit a cell never changes: the table, the peek and the Blob tab share one read.
        if let Some(commit) = snapshot.clone() {
            let key = (cell.clone(), commit, self.schema_epoch);
            return self.share(
                |session| &mut session.pinned_blobs,
                key,
                move |session, _, cx| session.blob_meta_unpinned(cell, snapshot, cx),
                |_, _| {},
                cx,
            );
        }
        self.blob_meta_unpinned(cell, snapshot, cx)
    }

    fn blob_meta_unpinned(
        &mut self,
        cell: BlobCell,
        snapshot: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<BlobMeta, ReadError>> {
        let client = self.client(cx);
        let graph = self.graph_id.clone();
        let branch = self.branch.clone();
        let request = async move {
            let at = match &snapshot {
                Some(commit) => BlobAt::Snapshot(commit),
                None => BlobAt::Branch(&branch),
            };
            client.blob_head(&graph, &cell, at).await
        };
        self.counted_read(request, cx)
    }

    /// A page of `branch`'s history, newest first ([`GraphClient::commit_page`]): its newest
    /// `limit` commits, or the next ones from `page_token`. Not kept: each page is one read, which
    /// costs only the history it returns.
    pub fn commit_page(
        &mut self,
        branch: &str,
        limit: u32,
        page_token: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<graph_client::api::CommitListOutput, ReadError>> {
        let client = self.client(cx);
        let graph = self.graph_id.clone();
        let branch = branch.to_owned();
        self.counted_read(
            async move {
                client
                    .commit_page(&graph, &branch, limit, page_token.as_deref())
                    .await
            },
            cx,
        )
    }

    /// One commit by its id ([`GraphClient::commit`]), whichever branch made it: a merged-in
    /// commit, which the listing of the branch it was merged into names only by id. Not kept: a
    /// commit never changes, so the view that reads one keeps it.
    pub fn commit(
        &mut self,
        commit_id: &str,
        cx: &mut Context<Self>,
    ) -> Task<Result<CommitOutput, ReadError>> {
        let client = self.client(cx);
        let graph = self.graph_id.clone();
        let commit_id = commit_id.to_owned();
        self.counted_read(async move { client.commit(&graph, &commit_id).await }, cx)
    }

    /// `branch`'s newest commit, as a list of branches shows it: read alone (a page of one) and
    /// kept until a commit lands on the branch. A branch with no commit has none. `fresh`
    /// forgets what is held of the branch's history and reads it again now, as a Refresh does,
    /// to see commits made elsewhere.
    pub fn head_commit(
        &mut self,
        branch: &str,
        fresh: bool,
        cx: &mut Context<Self>,
    ) -> Task<Result<Option<CommitOutput>, ReadError>> {
        let key = branch.to_owned();
        if fresh {
            self.forget_branch(&key);
        }
        let branch = key.clone();
        self.share(
            |session| &mut session.head_commits,
            key,
            move |session, _, cx| {
                let client = session.client(cx);
                let graph = session.graph_id.clone();
                let read = session.counted_read(
                    async move { client.commit_page(&graph, &branch, 1, None).await },
                    cx,
                );
                cx.spawn(async move |_, _| Ok(read.await?.commits.into_iter().next()))
            },
            |_, _| {},
            cx,
        )
    }

    /// One page of a commit's changes against its first parent, from `page_token` (the first
    /// page when `None`).
    pub fn commit_changes(
        &mut self,
        commit_id: &str,
        page_token: Option<String>,
        limit: u32,
        cx: &mut Context<Self>,
    ) -> Task<Result<CommitChanges, ReadError>> {
        let page = (commit_id.to_owned(), page_token.clone());
        if let Some(&whole) = self.whole_commit_pages.get(&page)
            && whole < limit
        {
            match self
                .commit_change_pages
                .get(&(page.0.clone(), page.1.clone(), whole))
            {
                Some(Held::Done(held)) => return Task::ready(Ok(held)),
                Some(Held::Pending(_)) => {}
                None => {
                    self.whole_commit_pages.remove(&page);
                }
            }
        }
        let key = (commit_id.to_owned(), page_token.clone(), limit);
        let commit = commit_id.to_owned();
        self.share(
            |session| &mut session.commit_change_pages,
            key,
            move |session, _, cx| {
                let (client, graph) = (session.client(cx), session.graph_id.clone());
                session.counted_read(
                    async move {
                        client
                            .commit_changes(&graph, &commit, page_token.as_deref(), limit)
                            .await
                    },
                    cx,
                )
            },
            |session, (commit, token, limit)| {
                let whole = session
                    .commit_change_pages
                    .peek(&(commit.clone(), token.clone(), *limit))
                    .is_some_and(|page| page.next_page_token.is_none());
                if !whole {
                    return;
                }
                if session.whole_commit_pages.len() >= COMMIT_CHANGE_PAGES {
                    session.whole_commit_pages.retain(|(commit, token), limit| {
                        session
                            .commit_change_pages
                            .holds(&(commit.clone(), token.clone(), *limit))
                    });
                }
                session
                    .whole_commit_pages
                    .entry((commit.clone(), token.clone()))
                    .and_modify(|held| *held = (*held).min(*limit))
                    .or_insert(*limit);
            },
            cx,
        )
    }

    /// The commit a merge of `source` into `target` would classify against, and both heads.
    pub fn merge_base(
        &mut self,
        source: &str,
        target: &str,
        cx: &mut Context<Self>,
    ) -> Task<Result<MergeBaseOutput, ReadError>> {
        let (client, graph) = (self.client(cx), self.graph_id.clone());
        let (source, target) = (source.to_owned(), target.to_owned());
        self.counted_read(
            async move { client.merge_base(&graph, &source, &target).await },
            cx,
        )
    }

    /// What merging `source` into `target` would do now, or its conflicts: a dry run, which
    /// writes nothing and takes no write gate, so it is a read here, not an outcome.
    pub fn merge_preview(
        &mut self,
        source: &str,
        target: &str,
        cx: &mut Context<Self>,
    ) -> Task<Result<graph_client::MergePreview, ReadError>> {
        let (client, graph) = (self.client(cx), self.graph_id.clone());
        let (source, target) = (source.to_owned(), target.to_owned());
        self.counted_read(
            async move { client.merge_preview(&graph, &source, &target).await },
            cx,
        )
    }

    /// One page of `branch`'s change feed: from `start` (`now`, `beginning` or
    /// `after:<commit_id>`), or from `page_token` to continue.
    pub fn changes(
        &mut self,
        branch: &str,
        start: Option<String>,
        page_token: Option<String>,
        limit: u32,
        cx: &mut Context<Self>,
    ) -> Task<Result<ChangeFeedOutput, ReadError>> {
        let (client, graph, branch) = (self.client(cx), self.graph_id.clone(), branch.to_owned());
        self.counted_read(
            async move {
                let at = match (&page_token, &start) {
                    (Some(token), _) => FeedAt::Page(token),
                    (None, Some(start)) => FeedAt::Start(start),
                    (None, None) => FeedAt::Start("now"),
                };
                client.changes(&graph, &branch, at, &[], limit).await
            },
            cx,
        )
    }

    /// One page of the net changes from commit `from` to commit `to`.
    pub fn diff(
        &mut self,
        from: &str,
        to: &str,
        page_token: Option<String>,
        limit: u32,
        cx: &mut Context<Self>,
    ) -> Task<Result<DiffOutput, ReadError>> {
        let (client, graph) = (self.client(cx), self.graph_id.clone());
        let (from, to) = (from.to_owned(), to.to_owned());
        self.counted_read(
            async move {
                client
                    .diff(&graph, &from, &to, page_token.as_deref(), limit)
                    .await
            },
            cx,
        )
    }

    /// The server's health and readiness, the graphs it serves, and `branch`'s snapshot: what
    /// the storage inspector shows, read together.
    pub fn server_state(&mut self, branch: &str, cx: &mut Context<Self>) -> Task<ServerState> {
        let client = self.client(cx);
        // Each is a read of its own, counted and noted as one.
        let health = {
            let client = client.clone();
            self.counted_read(async move { client.health().await }, cx)
        };
        let readiness = {
            let client = client.clone();
            self.counted_read(async move { client.readiness().await }, cx)
        };
        let graphs = self.counted_read(async move { client.graphs().await }, cx);
        let snapshot = self.branch_snapshot(branch, cx);
        cx.background_spawn(async move {
            ServerState {
                health: health.await,
                readiness: readiness.await,
                graphs: graphs.await,
                snapshot: snapshot.await,
            }
        })
    }

    /// `branch`'s storage snapshot: its tables, versions and row counts.
    pub fn branch_snapshot(
        &mut self,
        branch: &str,
        cx: &mut Context<Self>,
    ) -> Task<Result<SnapshotOutput, ReadError>> {
        let (client, graph, branch) = (self.client(cx), self.graph_id.clone(), branch.to_owned());
        self.counted_read(async move { client.snapshot(&graph, &branch).await }, cx)
    }

    /// How many neighbours `id` has through `group`, at `snapshot` (the branch head when
    /// `None`), with the commit the count holds at: the one asked for, or the one the read
    /// answered at, for a count asked for at the head. A view pinned by its first answer
    /// compares it with its pin, as it does every other answer. Counts asked for in the same
    /// turn for one node type, relationship and commit are read together, as
    /// `builder::page_degrees` reads of up to a hundred nodes in id order (`crate::degrees`);
    /// an asker that stops waiting is left out, and a read nobody waits for any more is
    /// stopped. Views ask through [`Self::degree_in`], which notes what the count depends on.
    pub(crate) fn degree_at(
        &mut self,
        type_name: &str,
        group: &RelationshipGroup,
        id: &str,
        snapshot: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<(u64, Option<String>), String>> {
        if let Some(commit) = &snapshot {
            let at = DegreeAt {
                type_name: type_name.to_owned(),
                group: group.clone(),
                id: id.to_owned(),
                commit: commit.clone(),
                schema: self.schema_epoch,
            };
            if let Some(n) = self.degree_kept(&at) {
                return Task::ready(Ok((n, Some(commit.clone()))));
            }
        }
        let (tx, rx) = oneshot::channel();
        let key = DegreeBatchKey {
            type_name: type_name.to_owned(),
            group: group.clone(),
            snapshot,
            branch: self.branch.clone(),
        };
        if self.degrees.add(key, id.to_owned(), tx) {
            let session = cx.entity().downgrade();
            cx.defer(move |cx| {
                session
                    .update(cx, |session, cx| session.flush_degrees(cx))
                    .ok();
            });
        }
        cx.background_spawn(async move {
            rx.await
                .unwrap_or_else(|_| Err("The count was not read.".to_owned()))
        })
    }

    fn flush_degrees(&mut self, cx: &mut Context<Self>) {
        let schema = self.schema_model();
        for (key, mut nodes) in self.degrees.take(DEGREE_BATCH) {
            nodes.retain_mut(|(_, waiters)| {
                waiters.retain(|waiter| !waiter.is_canceled());
                !waiters.is_empty()
            });
            if nodes.is_empty() {
                continue;
            }
            // Asked for at the head of a branch the session has since left: its head is not
            // this branch's, and the asker reads again on the move.
            if key.snapshot.is_none() && key.branch != self.branch {
                for (_, waiters) in nodes {
                    for waiter in waiters {
                        waiter
                            .send(Err(
                                "The branch changed while the count was asked for.".into()
                            ))
                            .ok();
                    }
                }
                continue;
            }
            let ids: Vec<String> = nodes.iter().map(|(id, _)| id.clone()).collect();
            let query = match &schema {
                Some(schema) => builder::page_degrees(schema, &key.type_name, &key.group, &ids)
                    .map_err(|error| error.to_string()),
                None => Err("The schema is still loading.".to_owned()),
            };
            let query = match query {
                Ok(query) => query,
                Err(error) => {
                    for (_, waiters) in nodes {
                        for waiter in waiters {
                            waiter.send(Err(error.clone())).ok();
                        }
                    }
                    continue;
                }
            };
            let read = self.read(&query, key.snapshot.clone(), cx);
            cx.background_spawn(async move {
                // Dropping the read when every asker has stopped waiting stops it.
                let result = {
                    let gone = futures::future::join_all(
                        nodes
                            .iter_mut()
                            .flat_map(|(_, waiters)| waiters.iter_mut())
                            .map(|waiter| waiter.cancellation()),
                    );
                    match select(read, std::pin::pin!(gone)).await {
                        Either::Left((result, _)) => result,
                        Either::Right(_) => return,
                    }
                };
                // Each count with the commit the read answered at: the one asked for, or the
                // head it read.
                let answer = result
                    .map_err(|error| error.to_string())
                    .and_then(|output| {
                        let counts = page_counts(&output.rows)
                            .ok_or_else(|| "The server's counts could not be read.".to_owned())?;
                        Ok((counts, output.graph_commit_id.clone()))
                    });
                for (id, waiters) in nodes {
                    let count = answer
                        .as_ref()
                        .map(|(counts, commit)| {
                            (counts.get(&id).copied().unwrap_or(0), commit.clone())
                        })
                        .map_err(Clone::clone);
                    for waiter in waiters {
                        waiter.send(count.clone()).ok();
                    }
                }
            })
            .detach();
        }
    }

    /// Bytes `range` of a managed Blob value, for a preview.
    pub fn blob_bytes(
        &mut self,
        cell: BlobCell,
        snapshot: Option<String>,
        range: std::ops::Range<u64>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<u8>, ReadError>> {
        let client = self.client(cx);
        let graph = self.graph_id.clone();
        let branch = self.branch.clone();
        self.counted_read(
            async move {
                let at = match &snapshot {
                    Some(commit) => BlobAt::Snapshot(commit),
                    None => BlobAt::Branch(&branch),
                };
                client.blob_bytes(&graph, &cell, at, range).await
            },
            cx,
        )
    }

    /// What a Blob cell holds at `branch`'s head, read now, whichever branch the session shows:
    /// what a write of the cell on that branch is bound to.
    pub fn blob_meta_on(
        &mut self,
        branch: &str,
        cell: BlobCell,
        cx: &mut Context<Self>,
    ) -> Task<Result<BlobMeta, ReadError>> {
        let client = self.client(cx);
        let graph = self.graph_id.clone();
        let branch = branch.to_owned();
        self.counted_read(
            async move {
                client
                    .blob_head(&graph, &cell, BlobAt::Branch(&branch))
                    .await
            },
            cx,
        )
    }

    /// Stores `bytes` as a Blob cell's value on `branch` (spec 004), bound by `if_match` to the
    /// value it replaces ([`GraphClient::blob_put`]). One of Studio's own writes: sent once, its
    /// outcome logged as `description` (never the bytes), and its commit is the branch's head.
    pub fn blob_put_on(
        &mut self,
        branch: &str,
        cell: BlobCell,
        if_match: Option<String>,
        bytes: Vec<u8>,
        description: String,
        cx: &mut Context<Self>,
    ) -> Task<WriteOutcome<graph_client::api::BlobWriteOutput>> {
        let client = self.client(cx);
        let (graph, sent_to) = (self.graph_id.clone(), branch.to_owned());
        let request = async move {
            client
                .blob_put(&graph, &sent_to, &cell, if_match.as_deref(), bytes)
                .await
        };
        self.send_blob_write(branch, description, request, cx)
    }

    /// Sets a nullable Blob cell to null on `branch`, bound as [`Self::blob_put_on`] binds. A
    /// cell already null publishes nothing, and moves no head.
    pub fn blob_clear_on(
        &mut self,
        branch: &str,
        cell: BlobCell,
        if_match: Option<String>,
        description: String,
        cx: &mut Context<Self>,
    ) -> Task<WriteOutcome<graph_client::api::BlobWriteOutput>> {
        let client = self.client(cx);
        let (graph, sent_to) = (self.graph_id.clone(), branch.to_owned());
        let request = async move {
            client
                .blob_clear(&graph, &sent_to, &cell, if_match.as_deref())
                .await
        };
        self.send_blob_write(branch, description, request, cx)
    }

    /// Runs a Blob write on `branch` as every write of the session runs ([`Self::run_write`]).
    fn send_blob_write(
        &mut self,
        branch: &str,
        description: String,
        request: impl Future<Output = WriteOutcome<graph_client::api::BlobWriteOutput>> + 'static,
        cx: &mut Context<Self>,
    ) -> Task<WriteOutcome<graph_client::api::BlobWriteOutput>> {
        let branch = branch.to_owned();
        self.run_write(
            request,
            |output: &graph_client::api::BlobWriteOutput| output.commit.as_ref(),
            move |_, _, _| WriteLog::new(branch, description),
            |_, _, _| {},
            cx,
        )
    }

    /// Runs a mutation on `branch`. `if_commit` binds it to the commit it was read at, and its
    /// outcome offers `follow_up` once it commits. The outcome is logged; nothing is retried.
    /// The branch is the caller's: each write of a chain goes where the first went, and a
    /// dialog's where its checks read, though the session moved to another branch meanwhile.
    pub fn write_on(
        &mut self,
        branch: &str,
        query: &BuiltQuery,
        if_commit: Option<String>,
        description: String,
        follow_up: Option<FollowUp>,
        cx: &mut Context<Self>,
    ) -> Task<WriteOutcome<ChangeOutput>> {
        let request = ChangeRequest {
            query: query.text.clone(),
            name: Some(query.name.clone()),
            params: query.params_value(),
            branch: Some(branch.to_owned()),
        };
        self.send_write(request, if_commit, description, follow_up, cx)
    }

    pub fn write_request(
        &mut self,
        request: ChangeRequest,
        if_commit: Option<String>,
        description: String,
        cx: &mut Context<Self>,
    ) -> Task<WriteOutcome<ChangeOutput>> {
        self.send_write(request, if_commit, description, None, cx)
    }

    fn send_write(
        &mut self,
        request: ChangeRequest,
        if_commit: Option<String>,
        description: String,
        follow_up: Option<FollowUp>,
        cx: &mut Context<Self>,
    ) -> Task<WriteOutcome<ChangeOutput>> {
        let client = self.client(cx);
        let graph = self.graph_id.clone();
        // A request naming no branch is a branch or commit statement, whose target is in its
        // text: the answer names the branch it changed, and without an answer the statement
        // says which it writes. Only a commit statement's is unknown until the server answers
        // (the proposal recorded it), and that one is logged on the session's branch.
        let statement = request.branch.is_none();
        let asked = match &request.branch {
            Some(branch) => branch.clone(),
            None => gq_language::check::statement_branch(&request.query)
                .unwrap_or_else(|| self.branch.clone()),
        };
        let send = async move { client.mutate(&graph, &request, if_commit.as_deref()).await };
        self.run_write(
            send,
            |output: &ChangeOutput| output.commit.as_ref(),
            move |_, outcome, _| {
                let branch = match outcome {
                    WriteOutcome::Committed(output) if statement && !output.branch.is_empty() => {
                        output.branch.clone()
                    }
                    _ => asked,
                };
                WriteLog {
                    follow_up,
                    ..WriteLog::new(branch, description)
                }
            },
            // A statement may have made or deleted a branch: the list is read again, and a
            // session on a branch it deleted moves to main.
            move |this, _, cx| {
                if statement {
                    this.reload_branches(cx);
                }
            },
            cx,
        )
    }

    /// A write sent on the person's behalf outside this session's own calls, as the agent's
    /// CLI writes through Studio's gateway (spec 013), from the moment it is sent: it counts
    /// among the writes in flight until [`ExternalWrite::finish`] records its outcome with
    /// Studio's own writes, and views of the branch learn of its commit. Dropped without an
    /// outcome, as when Studio stops the write's task, it is recorded as unknown.
    /// `creates_branch` reads the branch list again once it went through.
    pub fn external_write(
        &mut self,
        branch: String,
        description: String,
        creates_branch: bool,
        cx: &mut Context<Self>,
    ) -> ExternalWrite {
        let (done, outcome) = oneshot::channel();
        self.writes_in_flight += 1;
        cx.emit(SessionEvent::ActivityChanged);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let outcome = outcome.await.unwrap_or_else(|_| OutcomeKind::Unknown {
                reason: "Studio stopped before the server answered.".into(),
            });
            this.update(cx, |this, cx| {
                let went_through = matches!(
                    outcome,
                    OutcomeKind::Committed { .. } | OutcomeKind::Held { .. }
                );
                let made = (creates_branch && went_through).then(|| branch.clone());
                this.finish_write(WriteLog::new(branch, description), outcome, None, cx);
                if let Some(made) = made {
                    this.reload_branches_after(made, cx);
                }
            })
            .ok();
        })
        .detach();
        ExternalWrite { done }
    }

    /// Adds a write's outcome to the log, which keeps the newest [`OUTCOMES`]. A merge that may
    /// still land is kept past them ([`OutcomeEntry::merge_may_land`]): the reviews of its
    /// branches keep Merge off by it until the target's history settles it. There is at most one
    /// for each source and target, since no review merges those branches again meanwhile.
    fn log_outcome(&mut self, log: WriteLog, outcome: OutcomeKind) {
        if self.outcomes.len() >= OUTCOMES {
            let mut over = self.outcomes.len() + 1 - OUTCOMES;
            self.outcomes.retain(|entry| {
                if over == 0 || entry.merge_may_land() {
                    return true;
                }
                over -= 1;
                false
            });
        }
        let WriteLog {
            branch,
            description,
            follow_up,
            merge,
        } = log;
        self.outcomes.push(OutcomeEntry {
            at: SystemTime::now(),
            branch,
            description,
            outcome,
            follow_up,
            merge,
        });
        self.outcomes_logged += 1;
    }

    /// The newest merge of `source` into `target` the log holds: its entry says whether that
    /// merge may still land ([`OutcomeEntry::merge_may_land`]).
    pub fn newest_merge(&self, source: &str, target: &str) -> Option<&OutcomeEntry> {
        self.outcomes.iter().rev().find(|entry| {
            entry.branch == target
                && entry
                    .merge
                    .as_ref()
                    .is_some_and(|merge| merge.source == source)
        })
    }

    /// The newest merge of `source` into `target` sent over `sent` the log holds.
    pub fn merge_sent(&self, source: &str, target: &str, sent: &MergeSent) -> Option<&MergeEntry> {
        self.outcomes
            .iter()
            .rev()
            .filter(|entry| entry.branch == target)
            .filter_map(|entry| entry.merge.as_ref())
            .find(|merge| merge.source == source && merge.sent == *sent)
    }

    /// Records on its entry what the target's history showed of the merge of `source` into
    /// `target` sent over `sent`, whose outcome is unknown: every review of those branches reads
    /// it there. A merge shown to have landed stays so, since a read that does not show it may
    /// have come before it landed. Answers what the entry holds now, `None` when the log holds no
    /// such merge.
    pub fn settle_merge(
        &mut self,
        source: &str,
        target: &str,
        sent: &MergeSent,
        settled: MergeSettled,
        cx: &mut Context<Self>,
    ) -> Option<MergeSettled> {
        let merge = self
            .outcomes
            .iter_mut()
            .rev()
            .filter(|entry| {
                entry.branch == target && matches!(entry.outcome, OutcomeKind::Unknown { .. })
            })
            .filter_map(|entry| entry.merge.as_mut())
            .find(|merge| merge.source == source && merge.sent == *sent)?;
        if merge.settled != Some(MergeSettled::Landed) && merge.settled != Some(settled) {
            merge.settled = Some(settled);
            cx.emit(SessionEvent::ActivityChanged);
            cx.notify();
        }
        merge.settled
    }

    fn finish_write(
        &mut self,
        log: WriteLog,
        outcome: OutcomeKind,
        parent: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.writes_in_flight = self.writes_in_flight.saturating_sub(1);
        let commit = match &outcome {
            OutcomeKind::Committed { commit } => commit.clone(),
            _ => None,
        };
        let branch = log.branch.clone();
        self.log_outcome(log, outcome);
        if let Some(commit) = commit {
            self.committed(branch, commit, parent, cx);
        }
        // A 403 since the access route last answered: the credential, or the policy, may have
        // changed, and the session's branch is read again.
        self.sync_access(cx);
        cx.emit(SessionEvent::ActivityChanged);
        cx.notify();
    }

    /// One of Studio's writes committed `commit` on `branch`, over `parent` when the answer
    /// says: it is the branch's head.
    fn committed(
        &mut self,
        branch: String,
        commit: String,
        parent: Option<String>,
        cx: &mut Context<Self>,
    ) {
        // The counts are the session's branch's: a commit elsewhere (an agent's branch) leaves
        // them as they are.
        if branch == self.branch {
            self.refresh_counts(cx);
        }
        self.forget_branch(&branch);
        cx.emit(SessionEvent::Committed {
            branch: branch.clone(),
        });
        self.saw_head(branch, commit, true, parent.as_deref(), cx);
    }

    /// Creates branch `name` where `start` says. It publishes no commit: the receipt names the
    /// commit the branch starts at, which is its head.
    pub fn create_branch(
        &mut self,
        name: String,
        start: BranchStart,
        cx: &mut Context<Self>,
    ) -> Task<WriteOutcome<BranchCreateOutput>> {
        let client = self.client(cx);
        let graph = self.graph_id.clone();
        let request = start.request(name.clone());
        let send = async move { client.create_branch(&graph, &request).await };
        let branch = name.clone();
        self.run_write(
            send,
            |_| None,
            move |this, outcome, _| {
                // A branch made again under a deleted one's name takes drafts again.
                if matches!(outcome, WriteOutcome::Committed(_)) {
                    this.gone_branches.remove(&name);
                }
                let description = match (&start, outcome) {
                    (BranchStart::Head(_), _) => format!("Create branch {name}"),
                    (BranchStart::Commit(_), WriteOutcome::Committed(created)) => format!(
                        "Create branch {name} at {}",
                        studio_ui::short_commit_id(&created.from_commit_id)
                    ),
                    (BranchStart::Commit(commit), _) => format!(
                        "Create branch {name} at {}",
                        studio_ui::short_commit_id(commit)
                    ),
                };
                WriteLog::new(name, description)
            },
            move |this, outcome, cx| {
                // The receipt names the new branch's head: the commit it starts at.
                if let WriteOutcome::Committed(created) = outcome {
                    this.saw_head(
                        branch.clone(),
                        created.from_commit_id.clone(),
                        false,
                        None,
                        cx,
                    );
                }
                this.reload_branches_after(branch, cx);
            },
            cx,
        )
    }

    pub fn delete_branch(
        &mut self,
        name: String,
        cx: &mut Context<Self>,
    ) -> Task<WriteOutcome<BranchDeleteOutput>> {
        let client = self.client(cx);
        let graph = self.graph_id.clone();
        let sent = name.clone();
        let send = async move { client.delete_branch(&graph, &sent).await };
        let deleted = name.clone();
        self.run_write(
            send,
            |_| None,
            move |this, outcome, cx| {
                // Only a branch that is gone moves the session and loses its drafts; a refused or
                // unknown delete leaves both where they are.
                if matches!(outcome, WriteOutcome::Committed(_)) {
                    if this.branch == name {
                        this.set_branch("main".into(), cx);
                    }
                    this.drop_drafts_of(&name, cx);
                }
                let description = format!("Delete branch {name}");
                WriteLog::new(name, description)
            },
            move |this, _, cx| this.reload_branches_after(deleted, cx),
            cx,
        )
    }

    /// Merges `source` into `target`, and deletes `source` after a merge that lands when
    /// `delete_branch` is set: the deletion is the server's, reported on the answer, and never
    /// undoes the merge. A session on the deleted branch moves to `target`. A merge that landed
    /// while the deletion did not is logged as the merge, with `source` left and the server's
    /// reason. The entry keeps the merge's source and `sent`, the heads it was sent over
    /// ([`OutcomeEntry::merge`]).
    pub fn merge(
        &mut self,
        source: String,
        target: String,
        sent: MergeSent,
        delete_branch: bool,
        cx: &mut Context<Self>,
    ) -> Task<WriteOutcome<BranchMergeOutput>> {
        let client = self.client(cx);
        let graph = self.graph_id.clone();
        let request = BranchMergeRequest {
            source: source.clone(),
            target: Some(target.clone()),
            delete_branch,
            dry_run: false,
        };
        let send = async move { client.merge(&graph, &request).await };
        let (logged_source, logged_target) = (source.clone(), target.clone());
        self.run_write(
            send,
            |output: &BranchMergeOutput| output.commit.as_ref(),
            move |_, outcome, _| {
                let (source, target) = (logged_source, logged_target);
                let merged = format!("Merge {source} into {target}");
                let description = match outcome {
                    WriteOutcome::Committed(BranchMergeOutput {
                        branch_deleted: Some(false),
                        branch_delete_error_details,
                        ..
                    }) => match branch_delete_error_details {
                        Some(details) => {
                            format!("{merged}; {source} was not deleted: {}", details.error)
                        }
                        None => format!("{merged}; {source} was not deleted"),
                    },
                    WriteOutcome::Committed(BranchMergeOutput {
                        branch_deleted: None,
                        ..
                    }) if delete_branch => {
                        format!("{merged}; the server did not say whether {source} was deleted")
                    }
                    // As it was asked for: done, refused or unknown.
                    _ if delete_branch => format!("{merged} and delete {source}"),
                    _ => merged,
                };
                WriteLog {
                    merge: Some(MergeEntry {
                        source,
                        sent,
                        settled: None,
                    }),
                    ..WriteLog::new(target, description)
                }
            },
            move |this, outcome, cx| {
                let deleted = matches!(
                    outcome,
                    WriteOutcome::Committed(BranchMergeOutput {
                        branch_deleted: Some(true),
                        ..
                    })
                );
                if deleted {
                    if this.branch == source {
                        this.set_branch(target, cx);
                    }
                    this.drop_drafts_of(&source, cx);
                    this.reload_branches_after(source, cx);
                }
            },
            cx,
        )
    }

    /// Runs one of the session's own writes, `send`, as every one runs (constitution III):
    /// counted in [`Self::writes_in_flight`] while out, sent once and never again, and run to
    /// its outcome whoever still waits for it ([`Self::to_outcome`]). When it ends, `log` names
    /// the branch it went to and says what it did, from its answer, after doing what must come
    /// before the outcome is logged; the outcome is logged by the commit `commit` finds in the
    /// answer ([`OutcomeKind::written`]): a published one becomes its branch's head, a proposal
    /// or a snapshot is logged as held; then `after` does the write's own follow-up.
    fn run_write<T: 'static>(
        &mut self,
        send: impl Future<Output = WriteOutcome<T>> + 'static,
        commit: for<'a> fn(&'a T) -> Option<&'a CommitOutput>,
        log: impl FnOnce(&mut Self, &WriteOutcome<T>, &mut Context<Self>) -> WriteLog + 'static,
        after: impl FnOnce(&mut Self, &WriteOutcome<T>, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> Task<WriteOutcome<T>> {
        self.writes_in_flight += 1;
        cx.emit(SessionEvent::ActivityChanged);
        let work = cx.spawn(async move |this, cx| {
            let outcome = send.await;
            let kind = outcome_kind(&outcome, commit);
            let parent = parent_of(&outcome, commit);
            this.update(cx, |this, cx| {
                let log = log(this, &outcome, cx);
                this.finish_write(log, kind, parent, cx);
                after(this, &outcome, cx);
            })
            .ok();
            outcome
        });
        Self::to_outcome(work, cx)
    }
}

/// What one write's outcome is logged with ([`GraphSession::run_write`]): the branch it went
/// to, what it did, what its toast offers once it commits, and a merge's own facts.
struct WriteLog {
    branch: String,
    description: String,
    follow_up: Option<FollowUp>,
    merge: Option<MergeEntry>,
}

impl WriteLog {
    fn new(branch: String, description: String) -> Self {
        Self {
            branch,
            description,
            follow_up: None,
            merge: None,
        }
    }
}

/// How a write ended, by the commit its answer names, which `commit` finds
/// ([`OutcomeKind::written`]). A write that made none (a branch created or deleted, a merge
/// already up to date, a mutation that matched nothing) is `Committed { commit: None }`: it
/// moves no head and shows no commit.
fn outcome_kind<T>(
    outcome: &WriteOutcome<T>,
    commit: impl Fn(&T) -> Option<&CommitOutput>,
) -> OutcomeKind {
    match outcome {
        WriteOutcome::Committed(value) => OutcomeKind::written(
            commit(value).map(|commit| (commit.graph_commit_id.clone(), commit.kind)),
        ),
        WriteOutcome::Refused(error) => OutcomeKind::Refused {
            message: error.body.error.clone(),
        },
        WriteOutcome::Unknown { reason } => OutcomeKind::Unknown {
            reason: reason.clone(),
        },
    }
}

/// The parent of the commit a write's answer names, which `commit` finds: the head below it
/// when it is published.
fn parent_of<T>(
    outcome: &WriteOutcome<T>,
    commit: impl Fn(&T) -> Option<&CommitOutput>,
) -> Option<String> {
    match outcome {
        WriteOutcome::Committed(value) => commit(value)?.parent_commit_id.clone(),
        _ => None,
    }
}

/// Branches as Studio lists them: `main` first, then by name.
fn branch_order(mut branches: Vec<String>) -> Vec<String> {
    branches.sort_by(|a, b| (a != "main").cmp(&(b != "main")).then(a.cmp(b)));
    branches
}

/// Opens a session on a graph a window shows.
pub fn open_session(
    connection: Entity<Connection>,
    graph_id: String,
    branch: String,
    cx: &mut App,
) -> Entity<GraphSession> {
    connection.update(cx, |connection, _| connection.remember_graph(&graph_id));
    cx.new(|cx| GraphSession::new(connection, graph_id, branch, cx))
}

/// Opens a session on a graph its window opens behind the one it shows: it reads nothing until
/// first shown ([`GraphSession::parked`]).
pub fn open_session_behind(
    connection: Entity<Connection>,
    graph_id: String,
    branch: String,
    cx: &mut App,
) -> Entity<GraphSession> {
    connection.update(cx, |connection, _| connection.remember_graph(&graph_id));
    cx.new(|cx| GraphSession::parked(connection, graph_id, branch, cx))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use gpui::TestAppContext;
    use http_client::http::Response;
    use http_client::{AsyncBody, FakeHttpClient};

    use super::*;
    use crate::connection::{ServerEntry, ServerSource};

    /// A scope on main showing answers that depend on these node and edge types.
    fn watching(session: &mut GraphSession, nodes: &[&str], edges: &[&str]) -> ReadScope {
        let scope = session.read_scope("main");
        let depends = Depends::Types {
            nodes: nodes.iter().map(|n| n.to_string()).collect(),
            edges: edges.iter().map(|e| e.to_string()).collect(),
        };
        session.note_in(&scope, depends, None);
        scope
    }

    /// A write that published no commit (a branch created or deleted, a merge already up to
    /// date) is done without one: it moves no head and offers no commit to view. One whose
    /// commit is a proposal or a snapshot is held, with its commit; one of a kind this build does
    /// not know has an unknown outcome, since whether a branch moved is not known.
    #[test]
    fn a_write_without_a_commit_names_none() {
        let written = |commit: Option<(&str, CommitKind)>| {
            OutcomeKind::written(commit.map(|(id, kind)| (id.to_owned(), kind)))
        };
        assert_eq!(written(None), OutcomeKind::Committed { commit: None });
        assert_eq!(
            written(Some(("", CommitKind::Published))),
            OutcomeKind::Committed { commit: None }
        );
        assert_eq!(
            written(Some(("c2", CommitKind::Published))),
            OutcomeKind::Committed {
                commit: Some("c2".into())
            }
        );
        assert_eq!(
            written(Some(("p1", CommitKind::Unpublished))),
            OutcomeKind::Held {
                commit: "p1".into(),
                held: HeldAs::Proposal
            }
        );
        assert_eq!(
            written(Some(("s1", CommitKind::Unattached))),
            OutcomeKind::Held {
                commit: "s1".into(),
                held: HeldAs::Snapshot
            }
        );
        assert!(matches!(
            written(Some(("o1", CommitKind::Other))),
            OutcomeKind::Unknown { reason } if reason.contains("o1")
        ));
    }

    #[gpui::test]
    async fn a_graph_that_answers_503_is_unavailable_until_it_answers_again(
        cx: &mut TestAppContext,
    ) {
        let ready = Arc::new(AtomicBool::new(false));
        let flag = ready.clone();
        let http = FakeHttpClient::create(move |_request| {
            let ready = flag.load(Ordering::SeqCst);
            async move {
                let (status, body) = if ready {
                    (200, r#"{"branches":["main"]}"#)
                } else {
                    (503, r#"{"error":"graph knowledge is loading"}"#)
                };
                Ok(Response::builder()
                    .status(status)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                    .body(AsyncBody::from(body))
                    .unwrap())
            }
        });
        let server = ServerEntry {
            name: "dev".into(),
            url: "http://127.0.0.1:8787".into(),
            source: ServerSource::Environment,
            known_graphs: Vec::new(),
        };
        let connection = cx.new(|_| Connection::new(server, Some("t".into()), http).unwrap());
        let session =
            cx.new(|cx| GraphSession::new(connection, "knowledge".into(), "main".into(), cx));
        cx.run_until_parked();
        session.read_with(cx, |session, _| {
            assert_eq!(
                session.unavailable.as_deref(),
                Some("graph knowledge is loading")
            )
        });

        // Nothing is asked again before the wait; after it, the answer clears the mark.
        ready.store(true, Ordering::SeqCst);
        cx.executor().advance_clock(RECHECK_FIRST / 2);
        cx.run_until_parked();
        session.read_with(cx, |session, _| assert!(session.unavailable.is_some()));
        cx.executor().advance_clock(RECHECK_FIRST);
        cx.run_until_parked();
        session.read_with(cx, |session, _| {
            assert_eq!(session.unavailable, None);
            assert_eq!(
                session.branches.loaded().cloned(),
                Some(vec!["main".to_owned()])
            );
        });
    }

    /// A session on `branch` of a server answering every request with `body`, a branch list.
    fn listing_session(
        body: impl Into<String>,
        branch: &str,
        cx: &mut TestAppContext,
    ) -> Entity<GraphSession> {
        let body: Arc<str> = body.into().into();
        let http = FakeHttpClient::create(move |_request| {
            let body = body.to_string();
            async move {
                Ok(Response::builder()
                    .status(200)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                    .body(AsyncBody::from(body))
                    .unwrap())
            }
        });
        let server = ServerEntry {
            name: "dev".into(),
            url: "http://127.0.0.1:8787".into(),
            source: ServerSource::Environment,
            known_graphs: Vec::new(),
        };
        let connection = cx.new(|_| Connection::new(server, Some("t".into()), http).unwrap());
        let branch = branch.to_owned();
        cx.new(|cx| GraphSession::new(connection, "knowledge".into(), branch, cx))
    }

    /// A session on a server answering each mutation and merge with the next of `answers`
    /// (status and body), and every other request with a branch list of main, `branch` and
    /// feature.
    fn answering_session(
        answers: Vec<(u16, Value)>,
        branch: &str,
        cx: &mut TestAppContext,
    ) -> Entity<GraphSession> {
        let answers = Arc::new(std::sync::Mutex::new(std::collections::VecDeque::from(
            answers,
        )));
        let listed = serde_json::json!({ "branches": ["main", branch, "feature"] });
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path();
            let write = path.ends_with("/mutate") || path.ends_with("/branches/merge");
            let (status, body) = match write.then(|| answers.lock().unwrap().pop_front()) {
                Some(Some(answer)) => answer,
                _ => (200, listed.clone()),
            };
            async move {
                Ok(Response::builder()
                    .status(status)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                    .body(AsyncBody::from(body.to_string()))
                    .unwrap())
            }
        });
        let server = ServerEntry {
            name: "dev".into(),
            url: "http://127.0.0.1:8787".into(),
            source: ServerSource::Environment,
            known_graphs: Vec::new(),
        };
        let connection = cx.new(|_| Connection::new(server, Some("t".into()), http).unwrap());
        let branch = branch.to_owned();
        cx.new(|cx| GraphSession::new(connection, "knowledge".into(), branch, cx))
    }

    /// A branch statement names its target only in its text: its outcome is the branch the
    /// answer names, not the session's, and the branch list is read again. Refused or unknown,
    /// it has no answer to name one, and is logged on the branch its text writes to: a merge's
    /// target, a deleted branch. A commit statement's target is only the server's to name, and
    /// such a one is logged on the session's branch.
    #[gpui::test]
    async fn a_statement_is_recorded_on_the_branch_it_changed(cx: &mut TestAppContext) {
        let committed = serde_json::json!({
            "branch": "release",
            "query_name": "merge",
            "affected_nodes": 0,
            "affected_edges": 0,
            "commit": { "graph_commit_id": "r9", "graph_manifest_version": 9, "kind": "published", "created_at": 0 },
        });
        let refused = serde_json::json!({
            "error": "policy denied",
            "code": "forbidden",
            "disposition": { "action": "refresh", "effects": "none" },
        });
        let failed = serde_json::json!({
            "error": "the server failed",
            "code": "internal",
            "disposition": { "action": "reconcile", "effects": "unknown" },
        });
        let session = answering_session(
            vec![
                (200, committed),
                (403, refused.clone()),
                (500, failed),
                (403, refused),
            ],
            "fix",
            cx,
        );
        cx.run_until_parked();
        for statement in [
            "branch merge feature into release",
            "branch merge feature into main",
            "branch delete feature",
            "commit publish \"01JCOMMIT\"",
        ] {
            let write = session.update(cx, |session, cx| {
                let request = ChangeRequest {
                    query: statement.into(),
                    name: None,
                    params: None,
                    branch: None,
                };
                session.write_request(request, None, "Run statement".into(), cx)
            });
            write.await;
            cx.run_until_parked();
        }
        session.read_with(cx, |session, _| {
            let logged: Vec<(&str, &OutcomeKind)> = session
                .outcomes
                .iter()
                .map(|entry| (entry.branch.as_str(), &entry.outcome))
                .collect();
            assert!(
                matches!(
                    logged.as_slice(),
                    [
                        ("release", OutcomeKind::Committed { .. }),
                        ("main", OutcomeKind::Refused { .. }),
                        ("feature", OutcomeKind::Unknown { .. }),
                        ("fix", OutcomeKind::Refused { .. }),
                    ]
                ),
                "{logged:?}"
            );
            assert_eq!(session.head("release"), Some("r9"));
            assert_ne!(session.head("main"), Some("r9"), "main did not move");
        });
    }

    /// A write held off every branch is logged as held, with its commit, and moves no head: a
    /// mutation `as commit` is a proposal, one `as unattached branch` a snapshot, and a merge
    /// statement `as commit` a proposal for its target. None of them reads as committed to the
    /// branch it was sent to.
    #[gpui::test]
    async fn a_held_write_is_logged_held_and_moves_no_head(cx: &mut TestAppContext) {
        let held = |branch: &str, id: &str, kind: &str| {
            let answer = serde_json::json!({
                "branch": branch,
                "query_name": "q",
                "affected_nodes": 1,
                "affected_edges": 0,
                "commit": { "graph_commit_id": id, "graph_manifest_version": 10_415_222_161_995_537_841_u64,
                            "kind": kind, "created_at": 0, "parent_commit_id": "m1" },
            });
            (200, answer)
        };
        let session = answering_session(
            vec![
                held("main", "p1", "unpublished"),
                held("main", "s1", "unattached"),
                held("release", "p2", "unpublished"),
            ],
            "main",
            cx,
        );
        cx.run_until_parked();
        let heard = Arc::new(std::sync::Mutex::new(Vec::new()));
        let committed = heard.clone();
        cx.update(|cx| {
            cx.subscribe(&session, move |_, event: &SessionEvent, _| {
                if let SessionEvent::Committed { branch } = event {
                    committed.lock().unwrap().push(branch.clone());
                }
            })
            .detach()
        });
        for (query, branch) in [
            (
                "query add() as commit { insert Person { slug: \"a\" } }",
                Some("main"),
            ),
            (
                "query snap() as unattached branch { insert Person { slug: \"b\" } }",
                Some("main"),
            ),
            ("branch merge feature into release as commit", None),
        ] {
            let write = session.update(cx, |session, cx| {
                let request = ChangeRequest {
                    query: query.into(),
                    name: None,
                    params: None,
                    branch: branch.map(str::to_owned),
                };
                session.write_request(request, None, "Write".into(), cx)
            });
            write.await;
            cx.run_until_parked();
        }
        session.read_with(cx, |session, _| {
            let logged: Vec<(&str, &OutcomeKind)> = session
                .outcomes
                .iter()
                .map(|entry| (entry.branch.as_str(), &entry.outcome))
                .collect();
            let proposal = |commit: &str| OutcomeKind::Held {
                commit: commit.into(),
                held: HeldAs::Proposal,
            };
            let snapshot = OutcomeKind::Held {
                commit: "s1".into(),
                held: HeldAs::Snapshot,
            };
            assert_eq!(
                logged,
                [
                    ("main", &proposal("p1")),
                    ("main", &snapshot),
                    ("release", &proposal("p2")),
                ]
            );
            for (branch, commit) in [("main", "p1"), ("main", "s1"), ("release", "p2")] {
                assert_ne!(session.head(branch), Some(commit), "{branch} moved");
            }
        });
        assert!(
            heard.lock().unwrap().is_empty(),
            "no branch heard of a commit"
        );
    }

    /// A merge asked to delete its branch is logged as what happened: deleted with it, or
    /// merged with the branch left and the server's reason, the session staying there. Its entry
    /// keeps the merge's source and the heads it was sent over as fields, and a merge that
    /// committed has nothing to settle.
    #[gpui::test]
    async fn a_merge_whose_deletion_was_refused_says_the_branch_stays(cx: &mut TestAppContext) {
        let merged = |deleted: bool| {
            let mut answer = serde_json::json!({
                "source": "feature",
                "target": "main",
                "outcome": "merged",
                "commit": { "graph_commit_id": "m9", "graph_manifest_version": 9, "kind": "published", "created_at": 0, "parent_commit_id": "m1", "merged_parent_commit_id": "f1" },
                "branch_deleted": deleted,
            });
            if !deleted {
                answer["branch_delete_error_details"] =
                    serde_json::json!({ "error": "feature is protected", "code": "forbidden" });
            }
            (200, answer)
        };
        let session = answering_session(vec![merged(false), merged(true)], "feature", cx);
        cx.run_until_parked();
        let sent = MergeSent {
            source_head: Some("f1".into()),
            target_head: Some("m1".into()),
        };
        let merge = |cx: &mut TestAppContext| {
            let write = session.update(cx, |session, cx| {
                session.merge("feature".into(), "main".into(), sent.clone(), true, cx)
            });
            cx.run_until_parked();
            futures::executor::block_on(write);
            session.update(cx, |session, cx| {
                let entry = session.outcomes.last().unwrap();
                assert_eq!(
                    entry.merge,
                    Some(MergeEntry {
                        source: "feature".into(),
                        sent: sent.clone(),
                        settled: None,
                    })
                );
                assert!(!entry.merge_may_land());
                let said = (
                    entry.branch.clone(),
                    entry.description.clone(),
                    session.branch.clone(),
                );
                assert_eq!(
                    session.settle_merge("feature", "main", &sent, MergeSettled::Landed, cx),
                    None,
                    "committed, not settled"
                );
                said
            })
        };
        assert_eq!(
            merge(cx),
            (
                "main".to_owned(),
                "Merge feature into main; feature was not deleted: feature is protected".to_owned(),
                "feature".to_owned()
            )
        );
        assert_eq!(
            merge(cx),
            (
                "main".to_owned(),
                "Merge feature into main and delete feature".to_owned(),
                "main".to_owned()
            )
        );
    }

    /// A node type and an edge type may share a name; each keeps its own count.
    #[gpui::test]
    async fn a_node_and_an_edge_of_one_name_keep_their_own_counts(cx: &mut TestAppContext) {
        // One answer for every request: the branch list and the snapshot read their fields.
        let dataset = |kind: &str, count: i64| {
            serde_json::json!({
                "entity_kind": kind,
                "type_name": "Knows",
                "dataset_path": "p",
                "published_dataset_version": 1,
                "entity_count": count,
            })
        };
        let body = serde_json::json!({
            "branches": ["main"],
            "graph_branch": "main",
            "graph_manifest_version": 1,
            "internal_schema_version": 14,
            "datasets": [dataset("node", 3), dataset("edge", 7)],
        })
        .to_string();
        let session = listing_session(body, "main", cx);
        cx.run_until_parked();
        session.read_with(cx, |session, _| {
            assert_eq!(session.count_of(EntityKind::Node, "Knows"), Some(3));
            assert_eq!(session.count_of(EntityKind::Edge, "Knows"), Some(7));
        });
    }

    /// A commit on the session's branch reads its counts again and shows the old ones until
    /// the new are in, and after a read of them that fails; a commit on another branch (an
    /// agent's) reads none.
    #[gpui::test]
    async fn counts_stay_shown_while_a_commit_reads_them_again(cx: &mut TestAppContext) {
        use std::sync::atomic::AtomicUsize;
        let snapshots = Arc::new(AtomicUsize::new(0));
        let counted = snapshots.clone();
        // A gate the next snapshot read waits at, once one is set.
        let gate: Arc<std::sync::Mutex<Option<oneshot::Receiver<()>>>> = Arc::default();
        let held = gate.clone();
        // The snapshot read fails, once set.
        let failing = Arc::new(AtomicBool::new(false));
        let fails = failing.clone();
        let http = FakeHttpClient::create(move |request| {
            let mut wait = None;
            let snapshot = request.uri().path().ends_with("/snapshot");
            if snapshot {
                counted.fetch_add(1, Ordering::SeqCst);
                wait = held.lock().unwrap().take();
            }
            let fail = snapshot && fails.load(Ordering::SeqCst);
            async move {
                if let Some(wait) = wait {
                    wait.await.ok();
                }
                let (status, body) = if fail {
                    (
                        500,
                        serde_json::json!({ "error": "the snapshot could not be read" }),
                    )
                } else {
                    let dataset = serde_json::json!({
                        "entity_kind": "node",
                        "type_name": "Person",
                        "dataset_path": "p",
                        "published_dataset_version": 1,
                        "entity_count": 3,
                    });
                    (
                        200,
                        serde_json::json!({
                            "branches": ["main"],
                            "graph_branch": "main",
                            "graph_manifest_version": 1,
                            "internal_schema_version": 14,
                            "datasets": [dataset],
                        }),
                    )
                };
                Ok(Response::builder()
                    .status(status)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                    .body(AsyncBody::from(body.to_string()))
                    .unwrap())
            }
        });
        let server = ServerEntry {
            name: "dev".into(),
            url: "http://127.0.0.1:8787".into(),
            source: ServerSource::Environment,
            known_graphs: Vec::new(),
        };
        let connection = cx.new(|_| Connection::new(server, Some("t".into()), http).unwrap());
        let session =
            cx.new(|cx| GraphSession::new(connection, "knowledge".into(), "main".into(), cx));
        cx.run_until_parked();
        let read = snapshots.load(Ordering::SeqCst);
        let committed = |commit: &str| OutcomeKind::Committed {
            commit: Some(commit.into()),
        };
        session.update(cx, |session, cx| {
            session
                .external_write("agent/x".into(), "w".into(), false, cx)
                .finish(committed("c2"));
        });
        cx.run_until_parked();
        assert_eq!(
            snapshots.load(Ordering::SeqCst),
            read,
            "another branch's commit"
        );
        session.update(cx, |session, cx| {
            session
                .external_write("main".into(), "w".into(), false, cx)
                .finish(committed("c3"));
        });
        cx.run_until_parked();
        session.read_with(cx, |session, _| {
            assert!(
                matches!(session.datasets, Loadable::Loaded(_)),
                "still shown"
            );
        });
        assert_eq!(snapshots.load(Ordering::SeqCst), read + 1);

        // A burst of commits while the counts are read: the read under way is kept, and one
        // more follows it.
        let (open, wait) = oneshot::channel::<()>();
        *gate.lock().unwrap() = Some(wait);
        let before = snapshots.load(Ordering::SeqCst);
        for commit in ["c4", "c5", "c6"] {
            session.update(cx, |session, cx| {
                session
                    .external_write("main".into(), "w".into(), false, cx)
                    .finish(committed(commit));
            });
            cx.run_until_parked();
        }
        assert_eq!(
            snapshots.load(Ordering::SeqCst),
            before + 1,
            "one read under way"
        );
        open.send(()).ok();
        cx.run_until_parked();
        assert_eq!(
            snapshots.load(Ordering::SeqCst),
            before + 2,
            "then one more"
        );

        // The read after a commit fails: the counts shown are still the branch's.
        failing.store(true, Ordering::SeqCst);
        session.update(cx, |session, cx| {
            session
                .external_write("main".into(), "w".into(), false, cx)
                .finish(committed("c7"));
        });
        cx.run_until_parked();
        assert_eq!(snapshots.load(Ordering::SeqCst), before + 3);
        session.read_with(cx, |session, _| {
            assert!(matches!(session.datasets, Loadable::Loaded(_)));
            assert_eq!(session.count_of(EntityKind::Node, "Person"), Some(3));
        });
    }

    /// The counts name their commit once the branch's newest commit, read after the snapshot,
    /// is at the snapshot's manifest version. A branch numbers its own versions: moved to a
    /// branch whose snapshot is at the version main's was, the counts name no commit until that
    /// branch's newest is read, and never main's.
    #[gpui::test]
    async fn the_counts_commit_is_the_branchs_newest_at_the_snapshots_version(
        cx: &mut TestAppContext,
    ) {
        // Feature's first read of its newest commit waits until let go.
        let (release, wait) = oneshot::channel::<()>();
        let gate = Arc::new(std::sync::Mutex::new(Some(wait)));
        let pages: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let asked = pages.clone();
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            let query = request.uri().query().unwrap_or_default().to_owned();
            let main = query.contains("branch=main");
            let commits = path.ends_with("/commits");
            if commits {
                asked.lock().unwrap().push(query.clone());
            }
            let wait = (commits && !main)
                .then(|| gate.lock().unwrap().take())
                .flatten();
            async move {
                if let Some(wait) = wait {
                    wait.await.ok();
                }
                // Both branches' manifests are at version 5.
                let body = if path.ends_with("/snapshot") {
                    serde_json::json!({ "graph_branch": "main", "graph_manifest_version": 5,
                        "internal_schema_version": 16, "datasets": [] })
                } else if commits {
                    let id = if main { "c5" } else { "f1" };
                    let commit = serde_json::json!({ "graph_commit_id": id,
                        "graph_manifest_version": 5, "kind": "published", "created_at": 0 });
                    serde_json::json!({ "commits": [commit] })
                } else {
                    serde_json::json!({ "branches": ["main", "feature"] })
                };
                Ok(Response::builder()
                    .status(200)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                    .body(AsyncBody::from(body.to_string()))
                    .unwrap())
            }
        });
        let server = ServerEntry {
            name: "dev".into(),
            url: "http://127.0.0.1:8787".into(),
            source: ServerSource::Environment,
            known_graphs: Vec::new(),
        };
        let connection = cx.new(|_| Connection::new(server, Some("t".into()), http).unwrap());
        let session =
            cx.new(|cx| GraphSession::new(connection, "knowledge".into(), "main".into(), cx));
        cx.run_until_parked();
        let counts_commit = |cx: &mut TestAppContext| {
            session.read_with(cx, |session, _| session.counts_commit().map(str::to_owned))
        };
        assert_eq!(counts_commit(cx).as_deref(), Some("c5"));

        session.update(cx, |session, cx| session.set_branch("feature".into(), cx));
        cx.run_until_parked();
        assert_eq!(
            counts_commit(cx),
            None,
            "not main's c5, at the same version"
        );
        release.send(()).ok();
        cx.run_until_parked();
        assert_eq!(counts_commit(cx).as_deref(), Some("f1"));

        session.update(cx, |session, cx| session.set_branch("main".into(), cx));
        cx.run_until_parked();
        assert_eq!(counts_commit(cx).as_deref(), Some("c5"));
        assert_eq!(
            *pages.lock().unwrap(),
            [
                "branch=main&limit=1",
                "branch=feature&limit=1",
                "branch=main&limit=1"
            ],
            "one newest commit for each snapshot"
        );
    }

    /// A write sent outside the session's own calls counts as running until its outcome is
    /// recorded; dropped without one, as when Studio stops its task, it is recorded as unknown.
    #[gpui::test]
    async fn an_external_write_counts_until_its_outcome_and_is_unknown_when_dropped(
        cx: &mut TestAppContext,
    ) {
        let session = listing_session(r#"{"branches":["main"]}"#, "main", cx);
        let first = session.update(cx, |session, cx| {
            session.external_write("main".into(), "sent".into(), false, cx)
        });
        let second = session.update(cx, |session, cx| {
            session.external_write("main".into(), "stopped".into(), false, cx)
        });
        session.read_with(cx, |session, _| assert_eq!(session.writes_in_flight, 2));
        first.finish(OutcomeKind::Refused {
            message: "no".into(),
        });
        drop(second);
        cx.run_until_parked();
        session.read_with(cx, |session, _| {
            assert_eq!(session.writes_in_flight, 0);
            let outcomes: Vec<_> = session
                .outcomes
                .iter()
                .map(|entry| (entry.description.as_str(), &entry.outcome))
                .collect();
            assert!(matches!(
                outcomes.as_slice(),
                [
                    ("sent", OutcomeKind::Refused { .. }),
                    ("stopped", OutcomeKind::Unknown { .. })
                ]
            ));
        });
    }

    /// The activity log keeps the newest writes, and past them a merge that may still land, by
    /// which the reviews of its branches keep Merge off. What the target's history showed of a
    /// lost merge is recorded on its entry, a merge shown to have landed staying so; once
    /// settled, it goes as any other write.
    #[gpui::test]
    async fn the_activity_log_keeps_the_newest_writes(cx: &mut TestAppContext) {
        let lost = || (504, serde_json::json!({}));
        let session = answering_session(vec![lost(), lost()], "fix", cx);
        cx.run_until_parked();
        let sent = MergeSent {
            source_head: Some("f1".into()),
            target_head: Some("m1".into()),
        };
        for source in ["feature", "fix"] {
            let write = session.update(cx, |session, cx| {
                session.merge(source.into(), "main".into(), sent.clone(), false, cx)
            });
            cx.run_until_parked();
            futures::executor::block_on(write);
        }
        session.update(cx, |session, cx| {
            let lost = session.newest_merge("fix", "main").unwrap();
            assert!(matches!(lost.outcome, OutcomeKind::Unknown { .. }));
            assert!(lost.merge_may_land());
            let other = MergeSent::default();
            let mut settle =
                |sent: &MergeSent, settled| session.settle_merge("fix", "main", sent, settled, cx);
            assert_eq!(settle(&other, MergeSettled::Landed), None, "not sent so");
            assert_eq!(
                settle(&sent, MergeSettled::NotShown),
                Some(MergeSettled::NotShown)
            );
            assert_eq!(
                settle(&sent, MergeSettled::Landed),
                Some(MergeSettled::Landed)
            );
            assert_eq!(
                settle(&sent, MergeSettled::NotShown),
                Some(MergeSettled::Landed),
                "a merge that landed stays so"
            );
            assert!(
                !session
                    .newest_merge("fix", "main")
                    .unwrap()
                    .merge_may_land()
            );
            assert!(
                session
                    .newest_merge("feature", "main")
                    .unwrap()
                    .merge_may_land()
            );
        });
        for n in 0..=OUTCOMES {
            let refused = OutcomeKind::Refused {
                message: "no".into(),
            };
            session.update(cx, |session, cx| {
                session
                    .external_write("main".into(), format!("write {n}"), false, cx)
                    .finish(refused);
            });
            cx.run_until_parked();
        }
        session.read_with(cx, |session, _| {
            assert_eq!(session.outcomes.len(), OUTCOMES);
            assert!(session.outcomes[0].merge_may_land(), "the lost merge stays");
            assert!(session.newest_merge("feature", "main").is_some());
            assert!(
                session.newest_merge("fix", "main").is_none(),
                "the settled one went"
            );
            assert_eq!(session.outcomes[1].description, "write 2");
            assert_eq!(
                session.outcomes.last().unwrap().description,
                format!("write {OUTCOMES}")
            );
        });
    }

    /// Counts what the thread measuring allocates, less what it frees, around the system's
    /// allocator: what a decoded value holds, undisturbed by the tests running on other
    /// threads.
    mod counted {
        use std::alloc::{GlobalAlloc, Layout, System};
        use std::cell::Cell;

        pub struct Counting;

        thread_local! {
            static COUNTING: Cell<bool> = const { Cell::new(false) };
            static LIVE: Cell<isize> = const { Cell::new(0) };
        }

        fn count(bytes: isize) {
            let _ = COUNTING.try_with(|counting| {
                if counting.get() {
                    let _ = LIVE.try_with(|live| live.set(live.get() + bytes));
                }
            });
        }

        // SAFETY: each call goes to the system allocator as it came; counting touches only two
        // thread-local cells, which allocate nothing.
        unsafe impl GlobalAlloc for Counting {
            unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
                // SAFETY: the caller keeps `GlobalAlloc::alloc`'s contract, which this passes on.
                let ptr = unsafe { System.alloc(layout) };
                if !ptr.is_null() {
                    count(layout.size() as isize);
                }
                ptr
            }

            unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
                // SAFETY: as for `alloc`.
                let ptr = unsafe { System.alloc_zeroed(layout) };
                if !ptr.is_null() {
                    count(layout.size() as isize);
                }
                ptr
            }

            unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
                // SAFETY: `ptr` came from this allocator, so from the system's, with `layout`.
                unsafe { System.dealloc(ptr, layout) };
                count(-(layout.size() as isize));
            }

            unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
                // SAFETY: as for `dealloc`, with the caller's `new_size`.
                let moved = unsafe { System.realloc(ptr, layout, new_size) };
                if !moved.is_null() {
                    count(new_size as isize - layout.size() as isize);
                }
                moved
            }
        }

        /// What `make` made, and what it left allocated by this thread when it returned.
        pub fn live_after<T>(make: impl FnOnce() -> T) -> (T, isize) {
            LIVE.with(|live| live.set(0));
            COUNTING.with(|counting| counting.set(true));
            let made = make();
            COUNTING.with(|counting| counting.set(false));
            (made, LIVE.with(Cell::get))
        }
    }

    #[global_allocator]
    static ALLOCATOR: counted::Counting = counted::Counting;

    /// The memo weighs an answer as decoding it allocates, within a tenth: a page of node rows,
    /// which decodes to several times its text (11.9 times for the 100,000 rows spec 007
    /// measured), and a page of long texts, which decodes to about its length.
    #[test]
    fn an_answer_is_weighed_as_decoding_it_allocates() {
        // Objects are kept in order, as the estimate counts them.
        assert_ne!(
            std::mem::size_of::<serde_json::Map<String, Value>>(),
            std::mem::size_of::<BTreeMap<String, Value>>()
        );
        let node = |n: usize| {
            serde_json::json!({ "n": {
                "@id": format!("person-{n:06}"),
                "slug": format!("person-{n:06}"),
                "name": format!("Ada Lovelace {n}"),
                "email": format!("ada{n}@example.com"),
                "age": 20 + n % 60,
                "active": n.is_multiple_of(2),
                "score": 0.25,
                "tags": ["analyst", "engine"],
            }})
        };
        let long = |n: usize| {
            let body = "x".repeat(20_000);
            serde_json::json!({ "n": { "@id": format!("doc-{n}"), "body": body } })
        };
        let pages = [
            ((0..2_000).map(node).collect::<Vec<_>>(), 5.0..20.0),
            ((0..50).map(long).collect(), 1.0..1.2),
        ];
        for (rows, times_text) in pages {
            let text = serde_json::json!({
                "query_name": "studio_page",
                "target": { "branch": "main" },
                "row_count": rows.len(),
                "columns": ["n"],
                "rows": rows,
                "graph_commit_id": "c1",
            })
            .to_string();
            let (output, allocated) =
                counted::live_after(|| serde_json::from_str::<ReadOutput>(&text).unwrap());
            let (weighed, allocated) = (read_bytes(&output) as f64, allocated as f64);
            assert!(
                (weighed - allocated).abs() <= allocated / 10.0,
                "weighed {weighed}, allocated {allocated}"
            );
            let ratio = allocated / text.len() as f64;
            assert!(times_text.contains(&ratio), "{ratio} times the text");
        }
    }

    /// A family past the cap takes one place: the commits kept for the others stay.
    #[test]
    fn a_new_family_past_the_cap_takes_one_place() {
        let mut kept = HashMap::new();
        for family in 0..4 {
            remember(&mut kept, family, "c1", 4);
        }
        remember(&mut kept, 9, "c2", 4);
        assert_eq!(kept.len(), 4);
        assert_eq!(kept[&9], ["c2"]);
        assert_eq!(
            kept.values().filter(|commits| *commits == &["c1"]).count(),
            3
        );
        // A known family's newer commit takes no place.
        remember(&mut kept, 9, "c3", 4);
        assert_eq!(
            (kept.len(), kept[&9].clone()),
            (4, vec!["c2".to_owned(), "c3".to_owned()])
        );
    }

    /// A branch the branch list no longer holds cannot be read: the session moves to main and
    /// says so. A list asked for before the session last moved may not hold a branch made since,
    /// and moves nothing.
    #[gpui::test]
    async fn a_session_on_a_branch_that_is_gone_moves_to_main(cx: &mut TestAppContext) {
        let session = listing_session(r#"{"branches":["main","fix"]}"#, "gone", cx);
        let missing = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let heard = missing.clone();
        cx.update(|cx| {
            cx.subscribe(&session, move |_, event: &SessionEvent, _| {
                if let SessionEvent::BranchMissing { branch } = event {
                    heard.lock().unwrap().push(branch.clone());
                }
            })
            .detach()
        });
        cx.run_until_parked();
        session.read_with(cx, |session, _| assert_eq!(session.branch, "main"));
        assert_eq!(*missing.lock().unwrap(), ["gone"]);

        session.update(cx, |session, cx| {
            let before = session.branch_moves;
            session.set_branch("made".into(), cx);
            session.leave_missing_branch(before, cx);
            assert_eq!(session.branch, "made", "a list from before the move");
            session.set_branch("fix".into(), cx);
            session.leave_missing_branch(session.branch_moves, cx);
            assert_eq!(session.branch, "fix", "a branch the list holds");
        });
        assert_eq!(missing.lock().unwrap().len(), 1);
    }

    /// Drafts belong to the branch they were typed on: another branch shows its own, and the
    /// first one's wait until it is shown again (spec 014).
    #[gpui::test]
    async fn drafts_wait_with_their_branch(cx: &mut TestAppContext) {
        let session = listing_session(r#"{"branches":["main","fix"]}"#, "main", cx);
        let key = DraftKey {
            type_name: "Person".into(),
            id: "p1".into(),
            property: "name".into(),
        };
        let entry = DraftEntry {
            old: Value::from("Ada"),
            text: "Ada Lovelace".into(),
        };
        session.update(cx, |session, cx| {
            session.set_draft(key.clone(), entry.clone(), cx);
            assert_eq!(session.draft(&key), Some(&entry));
            session.set_branch("fix".into(), cx);
            assert_eq!(session.draft(&key), None, "another branch shows its own");
            assert_eq!(session.drafts().count(), 0);
            session.set_branch("main".into(), cx);
            assert_eq!(session.draft(&key), Some(&entry), "it waited");
            assert_eq!(session.remove_draft(&key, cx), Some(entry.clone()));
            assert_eq!(session.drafts().count(), 0);
        });

        // A write or read settles a draft only while it is as it went out: one retyped, or let
        // go, meanwhile stands.
        let retyped = DraftEntry {
            old: Value::from("Ada"),
            text: "Ada King".into(),
        };
        session.update(cx, |session, cx| {
            assert!(session.settle_draft_on("main", key.clone(), None, Some(entry.clone()), cx));
            assert_eq!(session.draft(&key), Some(&entry), "kept as it went out");
            session.set_draft(key.clone(), retyped.clone(), cx);
            assert!(!session.settle_draft_on("main", key.clone(), Some(&entry), None, cx));
            assert_eq!(session.draft(&key), Some(&retyped), "retyped meanwhile");
            assert!(session.settle_draft_on("main", key.clone(), Some(&retyped), None, cx));
            assert_eq!(session.draft(&key), None, "let go as it went out");
            let kept = Some(retyped.clone());
            assert!(!session.settle_draft_on("main", key.clone(), Some(&entry), kept, cx));
            assert_eq!(session.draft(&key), None, "let go meanwhile");
            assert_eq!(session.draft_on("fix", &key), None);
        });
    }

    /// A read at a commit is sent once: an identical read under way is shared, a later one is
    /// answered from memory. A read at the branch head is always sent.
    #[gpui::test]
    async fn reads_at_a_commit_are_sent_once(cx: &mut TestAppContext) {
        let queries = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let queries_seen = queries.clone();
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            let queries = queries_seen.clone();
            async move {
                let body = if path.ends_with("/query") {
                    queries.fetch_add(1, Ordering::SeqCst);
                    r#"{"query_name":"q","target":{"snapshot":"c1"},"row_count":1,"columns":["n"],"rows":[{"n":3}],"graph_commit_id":"c1"}"#
                } else {
                    r#"{"branches":["main"]}"#
                };
                Ok(Response::builder()
                    .status(200)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                    .body(AsyncBody::from(body))
                    .unwrap())
            }
        });
        let server = ServerEntry {
            name: "dev".into(),
            url: "http://127.0.0.1:8787".into(),
            source: ServerSource::Environment,
            known_graphs: Vec::new(),
        };
        let connection = cx.new(|_| Connection::new(server, Some("t".into()), http).unwrap());
        let session =
            cx.new(|cx| GraphSession::new(connection, "knowledge".into(), "main".into(), cx));
        cx.run_until_parked();
        let at_start = queries.load(Ordering::SeqCst);

        let query = BuiltQuery {
            name: "q".into(),
            text: "query q() { match { $p: Person } return { count($p) as n } }".into(),
            params: Default::default(),
            ..Default::default()
        };
        let (first, second) = session.update(cx, |session, cx| {
            (
                session.read(&query, Some("c1".into()), cx),
                session.read(&query, Some("c1".into()), cx),
            )
        });
        let (first, second) = (first.await.unwrap(), second.await.unwrap());
        assert_eq!(first.rows, second.rows);
        assert_eq!(
            queries.load(Ordering::SeqCst) - at_start,
            1,
            "shared while under way"
        );

        let later = session.update(cx, |session, cx| {
            session.read(&query, Some("c1".into()), cx)
        });
        let later = later.await.unwrap();
        assert_eq!(later.rows, first.rows);
        // The rows kept are handed over as they are, not copied for each reader.
        assert!(Arc::ptr_eq(&later.rows, &second.rows));
        assert_eq!(
            queries.load(Ordering::SeqCst) - at_start,
            1,
            "answered from memory"
        );

        let other = session.update(cx, |session, cx| {
            session.read(&query, Some("c2".into()), cx)
        });
        other.await.unwrap();
        assert_eq!(
            queries.load(Ordering::SeqCst) - at_start,
            2,
            "another commit is read"
        );

        for _ in 0..2 {
            let head = session.update(cx, |session, cx| session.read(&query, None, cx));
            head.await.unwrap();
        }
        assert_eq!(
            queries.load(Ordering::SeqCst) - at_start,
            4,
            "the branch head moves: every read is sent"
        );
    }

    /// A read at the commit the branch's head is at goes as a read of the branch, which the
    /// server answers from its warm caches, and counts only if it comes back at that commit.
    /// Once the branch has moved past it, the read is sent again pinned to the commit; the head
    /// moves only by the head watch.
    #[gpui::test]
    async fn a_read_at_the_head_goes_to_the_branch_and_checks_its_commit(cx: &mut TestAppContext) {
        use futures::AsyncReadExt as _;
        let served_head = Arc::new(std::sync::Mutex::new("c1"));
        let sent = Arc::new(std::sync::Mutex::new(
            Vec::<(Option<String>, Option<String>)>::new(),
        ));
        let (head_seen, sent_seen) = (served_head.clone(), sent.clone());
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            let (head, sent) = (head_seen.clone(), sent_seen.clone());
            async move {
                let body = if path.ends_with("/query") {
                    let mut bytes = Vec::new();
                    request.into_body().read_to_end(&mut bytes).await.ok();
                    let asked: Value = serde_json::from_slice(&bytes).unwrap();
                    let branch = asked["branch"].as_str().map(str::to_owned);
                    let snapshot = asked["snapshot"].as_str().map(str::to_owned);
                    sent.lock()
                        .unwrap()
                        .push((branch.clone(), snapshot.clone()));
                    let at = snapshot.clone().unwrap_or_else(|| match branch.as_deref() {
                        Some("review") => "r1".to_owned(),
                        _ => head.lock().unwrap().to_string(),
                    });
                    serde_json::json!({
                        "query_name": "q",
                        "target": { "branch": branch, "snapshot": snapshot },
                        "row_count": 1,
                        "columns": ["at"],
                        "rows": [{ "at": at }],
                        "graph_commit_id": at,
                    })
                    .to_string()
                } else {
                    r#"{"branches":["main"]}"#.to_owned()
                };
                Ok(Response::builder()
                    .status(200)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                    .body(AsyncBody::from(body))
                    .unwrap())
            }
        });
        let server = ServerEntry {
            name: "dev".into(),
            url: "http://127.0.0.1:8787".into(),
            source: ServerSource::Environment,
            known_graphs: Vec::new(),
        };
        let connection = cx.new(|_| Connection::new(server, Some("t".into()), http).unwrap());
        let session =
            cx.new(|cx| GraphSession::new(connection, "knowledge".into(), "main".into(), cx));
        cx.run_until_parked();
        session.update(cx, |session, _| {
            session.heads.insert("main".into(), "c1".into());
        });
        sent.lock().unwrap().clear();
        let query = |n: i64| BuiltQuery {
            name: "q".into(),
            text: "query q($n: I64) { match { $p: Person } return { count($p) as n } }".into(),
            params: [("n".to_owned(), Value::from(n))].into_iter().collect(),
            ..Default::default()
        };

        let read = session.update(cx, |session, cx| {
            session.read(&query(1), Some("c1".into()), cx)
        });
        let output = read.await.unwrap();
        assert_eq!(
            *sent.lock().unwrap(),
            vec![(Some("main".to_owned()), None)],
            "sent as a read of the branch"
        );
        assert_eq!(output.graph_commit_id.as_deref(), Some("c1"));
        assert_eq!(
            output.target.snapshot.as_deref(),
            Some("c1"),
            "answered as pinned"
        );
        assert_eq!(output.target.branch, None);

        *served_head.lock().unwrap() = "c2";
        sent.lock().unwrap().clear();
        let read = session.update(cx, |session, cx| {
            session.read(&query(2), Some("c1".into()), cx)
        });
        let output = read.await.unwrap();
        assert_eq!(
            *sent.lock().unwrap(),
            vec![
                (Some("main".to_owned()), None),
                (None, Some("c1".to_owned()))
            ],
            "the branch moved: read again at the commit"
        );
        assert_eq!(
            *output.rows,
            serde_json::json!([{ "at": "c1" }]),
            "the rows are the commit's"
        );
        session.read_with(cx, |session, _| {
            assert_eq!(
                session.head("main"),
                Some("c1"),
                "a read does not move the head"
            );
        });

        // At the head of another watched branch (a review's): read through that branch.
        session.update(cx, |session, _| {
            session.heads.insert("review".into(), "r1".into());
        });
        sent.lock().unwrap().clear();
        let read = session.update(cx, |session, cx| {
            session.read(&query(3), Some("r1".into()), cx)
        });
        let output = read.await.unwrap();
        assert_eq!(
            *sent.lock().unwrap(),
            vec![(Some("review".to_owned()), None)],
            "sent as a read of the review's branch"
        );
        assert_eq!(output.target.snapshot.as_deref(), Some("r1"));
    }

    /// Counts asked for one at a time in the same turn go out as one read per node type and
    /// relationship; asked again at the commit, they are answered from memory.
    #[gpui::test]
    async fn counts_asked_together_are_read_together(cx: &mut TestAppContext) {
        use futures::AsyncReadExt as _;
        use serde_json::Value;
        let reads: Arc<std::sync::Mutex<Vec<(String, Value)>>> = Arc::default();
        let seen = reads.clone();
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            let seen = seen.clone();
            async move {
                let mut bytes = Vec::new();
                request.into_body().read_to_end(&mut bytes).await.ok();
                let body = if path.ends_with("/schema") {
                    serde_json::json!({ "schema_source": "node Person {\n  slug: String @key\n}\nnode Company {\n  slug: String @key\n}\nedge Knows: Person -> Person\nedge WorksAt: Person -> Company\n" })
                } else if path.ends_with("/query") {
                    let request: Value = serde_json::from_slice(&bytes).unwrap();
                    let name = request["name"].as_str().unwrap_or_default().to_owned();
                    let ids = request["params"]["ids"].clone();
                    if name != "studio_head" {
                        seen.lock().unwrap().push((name, ids.clone()));
                    }
                    // Every node but p-2 has as many neighbours as its id's last digit.
                    let rows: Vec<Value> = ids
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .filter(|id| *id != "p-2")
                        .map(|id| {
                            let n = id[2..].parse::<u64>().unwrap();
                            serde_json::json!({ "id": id, "n": n })
                        })
                        .collect();
                    serde_json::json!({ "query_name": "studio_page_degrees", "target": { "snapshot": "c1" }, "row_count": rows.len(), "columns": ["id", "n"], "rows": rows, "graph_commit_id": "c1" })
                } else {
                    serde_json::json!({ "branches": ["main"] })
                };
                Ok(Response::builder()
                    .status(200)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                    .body(AsyncBody::from(body.to_string()))
                    .unwrap())
            }
        });
        let server = ServerEntry {
            name: "dev".into(),
            url: "http://127.0.0.1:8787".into(),
            source: ServerSource::Environment,
            known_graphs: Vec::new(),
        };
        let connection = cx.new(|_| Connection::new(server, Some("t".into()), http).unwrap());
        let session =
            cx.new(|cx| GraphSession::new(connection, "knowledge".into(), "main".into(), cx));
        cx.run_until_parked();
        assert!(session.read_with(cx, |session, _| session.schema_model().is_some()));
        let group = |edge: &str, other: &str| RelationshipGroup {
            edge: edge.into(),
            direction: gq_language::Direction::Outgoing,
            other_type: other.into(),
        };
        let ask = |cx: &mut TestAppContext, edge: &str, other: &str, ids: &[&str]| {
            let group = group(edge, other);
            session.update(cx, |session, cx| {
                ids.iter()
                    .map(|id| session.degree_at("Person", &group, id, Some("c1".into()), cx))
                    .collect::<Vec<_>>()
            })
        };

        // Three nodes' Knows and two nodes' WorksAt, asked one by one: two reads.
        let knows = ask(cx, "Knows", "Person", &["p-3", "p-1", "p-2"]);
        let works = ask(cx, "WorksAt", "Company", &["p-1", "p-3"]);
        let mut counts = Vec::new();
        for count in knows.into_iter().chain(works) {
            let (count, at) = count.await.unwrap();
            assert_eq!(at.as_deref(), Some("c1"), "held at the commit asked for");
            counts.push(count);
        }
        assert_eq!(counts, vec![3, 1, 0, 1, 3], "a node without a row has none");
        let mut sent = reads.lock().unwrap().clone();
        sent.sort_by_key(|(_, ids)| ids.to_string());
        assert_eq!(
            sent,
            vec![
                (
                    "studio_page_degrees".into(),
                    serde_json::json!(["p-1", "p-2", "p-3"])
                ),
                (
                    "studio_page_degrees".into(),
                    serde_json::json!(["p-1", "p-3"])
                ),
            ]
        );

        // The same counts at the same commit: from memory.
        let again = ask(cx, "Knows", "Person", &["p-2", "p-1", "p-3"]);
        for count in again {
            count.await.unwrap();
        }
        assert_eq!(reads.lock().unwrap().len(), 2);

        // A count asked for at the head of a branch the session leaves in the same turn is not
        // read at the new branch's head.
        let at_head = session.update(cx, |session, cx| {
            let count = session.degree_at("Person", &group("Knows", "Person"), "p-1", None, cx);
            session.set_branch("fix".into(), cx);
            count
        });
        let error = at_head.await.unwrap_err();
        assert!(error.contains("branch changed"), "{error}");
        assert_eq!(reads.lock().unwrap().len(), 2, "nothing was read for it");
    }

    /// The session asks after its branch's head, and each watched branch's, once the schema
    /// loads, at every poll while a window is active, and when a window becomes active after a
    /// poll came while none was. Another writer's commit lands like Studio's own; Studio's own
    /// commit is the head without asking. Each poll reads the branch list too, so a branch made
    /// elsewhere appears.
    /// A branch list read again after a branch was made or deleted is not overwritten by a
    /// poll's list asked for before it, which answers later.
    #[gpui::test]
    async fn a_polls_older_branch_list_does_not_replace_a_reloaded_one(cx: &mut TestAppContext) {
        let lists = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (gate_tx, gate_rx) = futures::channel::oneshot::channel::<()>();
        let gate = Arc::new(std::sync::Mutex::new(Some(gate_rx)));
        let (count, held) = (lists.clone(), gate.clone());
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            let (count, held) = (count.clone(), held.clone());
            async move {
                let body = if path.ends_with("/schema") {
                    serde_json::json!({ "schema_source": "node Person {\n  slug: String @key\n}\n" })
                } else if path.ends_with("/query") {
                    serde_json::json!({ "query_name": "studio_head", "target": { "branch": "main" }, "row_count": 0, "rows": [], "graph_commit_id": "c1" })
                } else if path.ends_with("/branches") {
                    // The first list, then the poll's (held until the reload landed), then the
                    // reload's, made after a branch was.
                    match count.fetch_add(1, Ordering::SeqCst) {
                        0 => serde_json::json!({ "branches": ["main"] }),
                        1 => {
                            let gate = held.lock().unwrap().take().unwrap();
                            gate.await.ok();
                            serde_json::json!({ "branches": ["main"] })
                        }
                        _ => serde_json::json!({ "branches": ["main", "made"] }),
                    }
                } else {
                    serde_json::json!({})
                };
                Ok(Response::builder()
                    .status(200)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                    .body(AsyncBody::from(body.to_string()))
                    .unwrap())
            }
        });
        let server = ServerEntry {
            name: "dev".into(),
            url: "http://127.0.0.1:8787".into(),
            source: ServerSource::Environment,
            known_graphs: Vec::new(),
        };
        let connection = cx.new(|_| Connection::new(server, Some("t".into()), http).unwrap());
        let session =
            cx.new(|cx| GraphSession::new(connection, "knowledge".into(), "main".into(), cx));
        cx.run_until_parked();
        let listed = |cx: &mut TestAppContext| {
            session.read_with(cx, |session, _| session.branches.loaded().cloned())
        };
        assert_eq!(listed(cx), Some(vec!["main".to_owned()]));

        // The poll asks; a branch is made meanwhile and the list is read again.
        session.update(cx, |session, cx| session.check_heads(cx));
        cx.run_until_parked();
        session.update(cx, |session, cx| session.reload_branches(cx));
        cx.run_until_parked();
        assert_eq!(listed(cx), Some(vec!["main".to_owned(), "made".to_owned()]));

        // The poll's answer, asked for before the branch was made, comes last.
        gate_tx.send(()).ok();
        cx.run_until_parked();
        assert_eq!(
            listed(cx),
            Some(vec!["main".to_owned(), "made".to_owned()]),
            "the newer list stays"
        );
    }

    /// A poll's branch list sent while a reload's was out, and answered first, is the newer:
    /// the reload's, landing after it, is not taken, so a branch only the poll's lists keeps
    /// its drafts.
    #[gpui::test]
    async fn an_older_branch_list_landing_last_does_not_replace_a_newer_one(
        cx: &mut TestAppContext,
    ) {
        let lists = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (gate_tx, gate_rx) = futures::channel::oneshot::channel::<()>();
        let gate = Arc::new(std::sync::Mutex::new(Some(gate_rx)));
        let (count, held) = (lists.clone(), gate.clone());
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            let (count, held) = (count.clone(), held.clone());
            async move {
                let body = if path.ends_with("/schema") {
                    serde_json::json!({ "schema_source": "node Person {\n  slug: String @key\n}\n" })
                } else if path.ends_with("/query") {
                    serde_json::json!({ "query_name": "studio_head", "target": { "branch": "main" }, "row_count": 0, "rows": [], "graph_commit_id": "c1" })
                } else if path.ends_with("/branches") {
                    // The first list; the reload's, held, from before a branch was made; the
                    // poll's, after.
                    match count.fetch_add(1, Ordering::SeqCst) {
                        0 => serde_json::json!({ "branches": ["main"] }),
                        1 => {
                            let gate = held.lock().unwrap().take().unwrap();
                            gate.await.ok();
                            serde_json::json!({ "branches": ["main"] })
                        }
                        _ => serde_json::json!({ "branches": ["main", "made"] }),
                    }
                } else {
                    serde_json::json!({})
                };
                Ok(Response::builder()
                    .status(200)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                    .body(AsyncBody::from(body.to_string()))
                    .unwrap())
            }
        });
        let server = ServerEntry {
            name: "dev".into(),
            url: "http://127.0.0.1:8787".into(),
            source: ServerSource::Environment,
            known_graphs: Vec::new(),
        };
        let connection = cx.new(|_| Connection::new(server, Some("t".into()), http).unwrap());
        let session =
            cx.new(|cx| GraphSession::new(connection, "knowledge".into(), "main".into(), cx));
        cx.run_until_parked();
        let listed = |cx: &mut TestAppContext| {
            session.read_with(cx, |session, _| session.branches.loaded().cloned())
        };
        assert_eq!(listed(cx), Some(vec!["main".to_owned()]));

        session.update(cx, |session, cx| session.reload_branches(cx));
        cx.run_until_parked();
        session.update(cx, |session, cx| session.check_heads(cx));
        cx.run_until_parked();
        let both = Some(vec!["main".to_owned(), "made".to_owned()]);
        assert_eq!(listed(cx), both, "the poll's");
        let key = DraftKey {
            type_name: "Person".into(),
            id: "p1".into(),
            property: "slug".into(),
        };
        let entry = DraftEntry {
            old: serde_json::json!("p1"),
            text: "p2".into(),
        };
        session.update(cx, |session, cx| {
            session.set_draft_on("made", key.clone(), entry, cx)
        });

        gate_tx.send(()).ok();
        cx.run_until_parked();
        assert_eq!(listed(cx), both, "the newer list stays");
        assert_eq!(
            session.read_with(cx, |session, _| session.drafts_on("made").count()),
            1,
            "its branch's drafts stay"
        );
    }

    /// A read at the head's commit goes to the branch; refused there because the branch is
    /// gone (404), it is sent again pinned to the commit. Any other refusal is the read's own,
    /// which the commit would answer alike: a 409 on this route is a full-text index to
    /// rebuild, and it is not sent again.
    #[gpui::test]
    async fn a_branch_read_refused_is_sent_pinned_only_when_the_branch_is_gone(
        cx: &mut TestAppContext,
    ) {
        use futures::AsyncReadExt as _;
        let refusal = Arc::new(std::sync::Mutex::new(409u16));
        let sent: Arc<std::sync::Mutex<Vec<bool>>> = Arc::default();
        let (status_seen, sent_seen) = (refusal.clone(), sent.clone());
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            let (refusal, sent) = (status_seen.clone(), sent_seen.clone());
            async move {
                let (status, body) = if path.ends_with("/query") {
                    let mut bytes = Vec::new();
                    request.into_body().read_to_end(&mut bytes).await.ok();
                    let asked: Value = serde_json::from_slice(&bytes).unwrap();
                    let pinned = asked["snapshot"].is_string();
                    sent.lock().unwrap().push(pinned);
                    if pinned {
                        let answer = serde_json::json!({ "query_name": "q",
                            "target": { "snapshot": "c1" }, "row_count": 0, "columns": [],
                            "rows": [], "graph_commit_id": "c1" });
                        (200, answer.to_string())
                    } else {
                        let status = *refusal.lock().unwrap();
                        let answer = serde_json::json!({ "error": "refused",
                            "code": if status == 404 { "not_found" } else { "conflict" } });
                        (status, answer.to_string())
                    }
                } else {
                    (200, r#"{"branches":["main"]}"#.to_owned())
                };
                Ok(Response::builder()
                    .status(status)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                    .body(AsyncBody::from(body))
                    .unwrap())
            }
        });
        let server = ServerEntry {
            name: "dev".into(),
            url: "http://127.0.0.1:8787".into(),
            source: ServerSource::Environment,
            known_graphs: Vec::new(),
        };
        let connection = cx.new(|_| Connection::new(server, Some("t".into()), http).unwrap());
        let session =
            cx.new(|cx| GraphSession::new(connection, "knowledge".into(), "main".into(), cx));
        cx.run_until_parked();
        session.update(cx, |session, _| {
            session.heads.insert("main".into(), "c1".into());
        });
        let query = |n: i64| BuiltQuery {
            name: "q".into(),
            text: "query q($n: I64) { match { $p: Person } return { count($p) as n } }".into(),
            params: [("n".to_owned(), Value::from(n))].into_iter().collect(),
            ..Default::default()
        };

        sent.lock().unwrap().clear();
        let read = session.update(cx, |session, cx| {
            session.read(&query(1), Some("c1".into()), cx)
        });
        let refused = read.await;
        assert!(
            matches!(&refused, Err(ReadError::Refused(refusal)) if refusal.status == 409),
            "{refused:?}"
        );
        assert_eq!(*sent.lock().unwrap(), vec![false], "not sent again");

        *refusal.lock().unwrap() = 404;
        sent.lock().unwrap().clear();
        let read = session.update(cx, |session, cx| {
            session.read(&query(2), Some("c1".into()), cx)
        });
        assert!(read.await.is_ok());
        assert_eq!(
            *sent.lock().unwrap(),
            vec![false, true],
            "the branch is gone: read at the commit"
        );
    }

    /// The node answers a page's rows give are kept within a bound of their own: however many
    /// pages are scrolled, an answer that was read stays.
    #[gpui::test]
    async fn seeded_answers_never_push_out_an_answer_that_was_read(cx: &mut TestAppContext) {
        use crate::test_server::followed_session;
        use serde_json::json;

        let (session, _) = followed_session(
            "node Person {\n  slug: String @key\n}\n",
            |_| json!([{ "n": 3 }]),
            cx,
        );
        cx.run_until_parked();
        let group = BuiltQuery {
            name: "g".into(),
            text: "query g() { match { $p: Person } return { count($p) as n } }".into(),
            ..Default::default()
        };
        let read = session.update(cx, |session, cx| {
            session.read(&group, Some("c1".into()), cx)
        });
        let page = read.await.unwrap();
        session.update(cx, |session, _| {
            let read_key = session.pinned_key(&group, "c1");
            assert!(session.pinned_reads.holds(&read_key));
            // Five memos' worth of node answers from scrolled pages.
            let nodes: Vec<(PinnedRead, ReadOutput)> = (0..PINNED_READS + SEEDED_READS)
                .map(|n| {
                    let node = BuiltQuery {
                        name: "studio_node".into(),
                        text: format!("query studio_node() {{ match {{ $n: Person }} return {{ $n }} limit {n} }}"),
                        ..Default::default()
                    };
                    (session.pinned_key(&node, "c1"), page.clone())
                })
                .collect();
            let seeds = Seeds::Nodes {
                type_name: "Person".into(),
            };
            let (epoch, credential) = (session.schema_epoch, session.credential);
            session.seed(&seeds, &page, nodes, epoch, credential);
            assert!(session.pinned_reads.holds(&read_key), "the read stays");
            assert_eq!(session.seeded_reads.len(), SEEDED_READS);
            assert!(session.seeded_commits.len() <= SEEDED_READS);
        });
    }

    /// A graph opened behind the one its window shows sends nothing, not its first reads nor
    /// a head probe or branch list at the polls, and holds no watch on its server; its first
    /// show reads the graph, and the schema's answer asks after the head.
    #[gpui::test]
    async fn a_graph_opened_behind_asks_nothing_until_first_shown(cx: &mut TestAppContext) {
        let (session, connection, asked, _) = parked_graph(cx);
        cx.run_until_parked();
        cx.add_empty_window()
            .update(|window, _| window.activate_window());
        cx.executor().advance_clock(HEAD_POLL);
        cx.run_until_parked();
        let _watch = session.update(cx, |session, cx| {
            session.check_heads(cx);
            session.window_activated(cx);
            session.watch_head("feature", cx)
        });
        cx.run_until_parked();
        assert!(asked.lock().unwrap().is_empty(), "{:?}", asked.lock());
        assert!(!connection.read_with(cx, |connection, _| connection.watched()));

        // A view wants the catalog while the graph is parked: it is read with the first reads.
        session.update(cx, |session, cx| session.want_stored_queries(cx));
        cx.run_until_parked();
        assert!(asked.lock().unwrap().is_empty(), "{:?}", asked.lock());

        session.update(cx, |session, cx| session.set_shown(true, cx));
        cx.run_until_parked();
        let sent = std::mem::take(&mut *asked.lock().unwrap());
        let times = |read: &str| sent.iter().filter(|sent| *sent == read).count();
        for read in ["schema", "snapshot", "queries", "access"] {
            assert_eq!(times(read), 1, "{read}: {sent:?}");
        }
        assert!(times("branches") >= 1, "{sent:?}");
        assert_eq!(
            times("studio_head"),
            2,
            "main and the watched branch: {sent:?}"
        );
        assert!(connection.read_with(cx, |connection, _| connection.watched()));
        session.read_with(cx, |session, _| {
            assert_eq!(session.count_of(EntityKind::Node, "Person"), Some(7));
            assert_eq!(session.head("main"), Some("c1"));
        });
    }

    /// The wait after a 503 ending while the graph is parked sends nothing; shown again, the
    /// graph is asked again, and what it holds (counts, the catalog, the branch list) stays
    /// shown while it is, and when the graph still answers 503.
    #[gpui::test]
    async fn a_503_recheck_waits_for_the_graph_to_show_and_keeps_what_it_holds(
        cx: &mut TestAppContext,
    ) {
        let (session, _, asked, failing) = parked_graph(cx);
        session.update(cx, |session, cx| {
            session.set_shown(true, cx);
            session.want_stored_queries(cx);
        });
        cx.run_until_parked();
        let held = |cx: &mut TestAppContext| {
            session.read_with(cx, |session, _| {
                (
                    session.count_of(EntityKind::Node, "Person"),
                    matches!(session.stored_queries, Loadable::Loaded(_)),
                    matches!(session.branches, Loadable::Loaded(_)),
                    session.unavailable.is_some(),
                )
            })
        };
        assert_eq!(held(cx), (Some(7), true, true, false));

        failing.store(true, Ordering::SeqCst);
        let query = BuiltQuery {
            name: "q".into(),
            text: "query q() { match { $p: Person } return { count($p) as n } }".into(),
            ..Default::default()
        };
        let read = session.update(cx, |session, cx| session.read(&query, None, cx));
        assert!(read.await.is_err());
        assert_eq!(held(cx), (Some(7), true, true, true));
        session.update(cx, |session, cx| session.set_shown(false, cx));
        asked.lock().unwrap().clear();
        cx.executor().advance_clock(RECHECK_FIRST);
        cx.run_until_parked();
        assert!(
            asked.lock().unwrap().is_empty(),
            "parked: {:?}",
            asked.lock()
        );

        session.update(cx, |session, cx| session.set_shown(true, cx));
        cx.run_until_parked();
        let sent = std::mem::take(&mut *asked.lock().unwrap());
        for read in ["schema", "branches", "snapshot", "queries"] {
            assert!(sent.iter().any(|sent| sent == read), "{read}: {sent:?}");
        }
        assert_eq!(held(cx), (Some(7), true, true, true), "kept through a 503");

        failing.store(false, Ordering::SeqCst);
        cx.executor().advance_clock(RECHECK_FIRST * 2);
        cx.run_until_parked();
        assert_eq!(held(cx), (Some(7), true, true, false), "answered again");
    }

    /// A graph opened behind its window's shown one ([`GraphSession::parked`]) on a server that
    /// names each request it is sent (the last part of its path, or the query's name) and
    /// answers 503 to all of them while `failing` holds.
    #[allow(clippy::type_complexity)]
    fn parked_graph(
        cx: &mut TestAppContext,
    ) -> (
        Entity<GraphSession>,
        Entity<Connection>,
        Arc<std::sync::Mutex<Vec<String>>>,
        Arc<AtomicBool>,
    ) {
        use futures::AsyncReadExt as _;
        use serde_json::json;
        let asked: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let failing = Arc::new(AtomicBool::new(false));
        let (log, down) = (asked.clone(), failing.clone());
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            let (log, down) = (log.clone(), down.clone());
            async move {
                let mut bytes = Vec::new();
                request.into_body().read_to_end(&mut bytes).await.ok();
                let asked: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
                let read = match asked["name"].as_str() {
                    Some(name) => name.to_owned(),
                    None => path.rsplit('/').next().unwrap_or_default().to_owned(),
                };
                log.lock().unwrap().push(read.clone());
                let (status, body) = if down.load(Ordering::SeqCst) {
                    (
                        503,
                        json!({ "error": "graph knowledge is loading", "code": "graph_unavailable" }),
                    )
                } else {
                    let body = match read.as_str() {
                        "schema" => {
                            json!({ "schema_source": "node Person {\n  slug: String @key\n}\n" })
                        }
                        "snapshot" => json!({ "graph_branch": "main", "graph_manifest_version": 3,
                            "internal_schema_version": 16, "datasets": [{ "entity_kind": "node",
                            "type_name": "Person", "dataset_path": "nodes/1",
                            "published_dataset_version": 2, "entity_count": 7 }] }),
                        "queries" => json!({ "queries": [] }),
                        "access" => json!({ "graph_id": "knowledge", "actor_id": "act-ada",
                            "graph_wide": ["read"], "branch": "main", "actions": ["read"] }),
                        "branches" => json!({ "branches": ["main", "feature"] }),
                        name => json!({ "query_name": name, "target": { "branch": "main" },
                            "row_count": 0, "columns": [], "rows": [], "graph_commit_id": "c1" }),
                    };
                    (200, body)
                };
                Ok(Response::builder()
                    .status(status)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                    .body(AsyncBody::from(body.to_string()))
                    .unwrap())
            }
        });
        let server = ServerEntry {
            name: "dev".into(),
            url: "http://127.0.0.1:8787".into(),
            source: ServerSource::Environment,
            known_graphs: Vec::new(),
        };
        let connection = cx.new(|_| Connection::new(server, Some("t".into()), http).unwrap());
        let session = cx.new(|cx| {
            GraphSession::parked(connection.clone(), "knowledge".into(), "main".into(), cx)
        });
        (session, connection, asked, failing)
    }

    #[gpui::test]
    async fn the_head_watch_sees_other_writers_commits_while_a_window_is_active(
        cx: &mut TestAppContext,
    ) {
        use futures::AsyncReadExt as _;
        use serde_json::Value;
        let heads: Arc<std::sync::Mutex<HashMap<String, String>>> = Arc::new(
            [("main", "c1"), ("feature", "f1")]
                .map(|(branch, head)| (branch.to_owned(), head.to_owned()))
                .into_iter()
                .collect::<HashMap<_, _>>()
                .into(),
        );
        let probes: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let listed: Arc<std::sync::Mutex<Vec<&'static str>>> =
            Arc::new(vec!["main", "feature"].into());
        let (known, asked, branch_list) = (heads.clone(), probes.clone(), listed.clone());
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            let (known, asked, branch_list) = (known.clone(), asked.clone(), branch_list.clone());
            async move {
                let mut bytes = Vec::new();
                request.into_body().read_to_end(&mut bytes).await.ok();
                let body = if path.ends_with("/schema") {
                    serde_json::json!({ "schema_source": "node Person {\n  slug: String @key\n}\n" })
                } else if path.ends_with("/query") {
                    let request: Value = serde_json::from_slice(&bytes).unwrap();
                    assert_eq!(request["name"], "studio_head");
                    let branch = request["branch"].as_str().unwrap().to_owned();
                    let head = known.lock().unwrap()[&branch].clone();
                    asked.lock().unwrap().push(branch);
                    serde_json::json!({ "query_name": "studio_head", "target": { "branch": "main" }, "row_count": 0, "rows": [], "graph_commit_id": head })
                } else if path.ends_with("/mutate") {
                    serde_json::json!({ "branch": "main", "query_name": "m", "affected_nodes": 1, "affected_edges": 0, "commit": { "graph_commit_id": "c9", "graph_branch": "main", "graph_manifest_version": 9, "kind": "published", "created_at": 9, "parent_commit_id": "c2" } })
                } else {
                    serde_json::json!({ "branches": *branch_list.lock().unwrap() })
                };
                Ok(Response::builder()
                    .status(200)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                    .body(AsyncBody::from(body.to_string()))
                    .unwrap())
            }
        });
        let server = ServerEntry {
            name: "dev".into(),
            url: "http://127.0.0.1:8787".into(),
            source: ServerSource::Environment,
            known_graphs: Vec::new(),
        };
        let connection = cx.new(|_| Connection::new(server, Some("t".into()), http).unwrap());
        let session = cx
            .new(|cx| GraphSession::new(connection.clone(), "knowledge".into(), "main".into(), cx));
        let events: Rc<std::cell::RefCell<Vec<SessionEvent>>> = Rc::default();
        let seen = events.clone();
        let _events = cx.update(|cx| {
            cx.subscribe(&session, move |_, event: &SessionEvent, _| {
                if matches!(
                    event,
                    SessionEvent::Committed { .. }
                        | SessionEvent::HeadMoved { .. }
                        | SessionEvent::BranchesChanged
                ) {
                    seen.borrow_mut().push(event.clone());
                }
            })
        });
        let head_of = |cx: &mut TestAppContext, branch: &str| {
            session.read_with(cx, |session, _| session.head(branch).map(str::to_owned))
        };
        let moved = |branch: &str, head: &str| SessionEvent::HeadMoved {
            branch: branch.into(),
            head: head.into(),
        };
        let landed = |branch: &str| SessionEvent::Committed {
            branch: branch.into(),
        };
        cx.run_until_parked();
        assert_eq!(head_of(cx, "main").as_deref(), Some("c1"));
        events
            .borrow_mut()
            .retain(|event| *event != SessionEvent::BranchesChanged);
        assert_eq!(
            events.take(),
            vec![moved("main", "c1")],
            "a head seen first is no commit"
        );

        // Another writer commits while no window is active: the poll asks nothing.
        heads.lock().unwrap().insert("main".into(), "c2".into());
        cx.executor().advance_clock(HEAD_POLL);
        cx.run_until_parked();
        assert_eq!(probes.lock().unwrap().len(), 1);
        assert_eq!(head_of(cx, "main").as_deref(), Some("c1"));

        // A window becomes active: asked at once; a second activation asks nothing more. The
        // branch list read again is unchanged: nothing is said about it.
        for _ in 0..2 {
            session.update(cx, |session, cx| session.window_activated(cx));
            cx.run_until_parked();
        }
        assert_eq!(probes.lock().unwrap().len(), 2);
        assert_eq!(events.take(), vec![landed("main"), moved("main", "c2")]);

        // A watched branch is asked after at once, alone, then with the session's own at each
        // poll. A branch made elsewhere appears at the poll.
        probes.lock().unwrap().clear();
        let watch = session.update(cx, |session, cx| session.watch_head("feature", cx));
        cx.run_until_parked();
        assert_eq!(head_of(cx, "feature").as_deref(), Some("f1"));
        assert_eq!(
            *probes.lock().unwrap(),
            vec!["feature"],
            "only the new branch"
        );
        cx.add_empty_window()
            .update(|window, _| window.activate_window());
        heads.lock().unwrap().insert("feature".into(), "f2".into());
        listed.lock().unwrap().push("spike");
        probes.lock().unwrap().clear();
        cx.executor().advance_clock(HEAD_POLL);
        cx.run_until_parked();
        let mut asked = probes.lock().unwrap().clone();
        asked.sort();
        assert_eq!(asked, vec!["feature", "main"]);
        assert_eq!(head_of(cx, "feature").as_deref(), Some("f2"));
        assert!(events.take().contains(&SessionEvent::BranchesChanged));
        assert_eq!(
            session.read_with(cx, |session, _| session.branches.loaded().cloned()),
            Some(vec!["main".into(), "feature".into(), "spike".into()])
        );

        // The watch dropped: the branch is no longer asked after, and its head is forgotten.
        drop(watch);
        probes.lock().unwrap().clear();
        cx.executor().advance_clock(HEAD_POLL);
        cx.run_until_parked();
        assert_eq!(*probes.lock().unwrap(), vec!["main"]);
        assert_eq!(head_of(cx, "feature"), None);

        // Studio's own commit: the head without asking, landing once.
        let query = BuiltQuery {
            name: "m".into(),
            text: "query m() { update Person set { slug: \"x\" } where @id = \"p\" }".into(),
            params: Default::default(),
            ..Default::default()
        };
        probes.lock().unwrap().clear();
        let write = session.update(cx, |session, cx| {
            session.write_on("main", &query, None, "Set slug".into(), None, cx)
        });
        write.await;
        cx.run_until_parked();
        assert!(probes.lock().unwrap().is_empty());
        assert_eq!(head_of(cx, "main").as_deref(), Some("c9"));
        assert_eq!(events.take(), vec![landed("main"), moved("main", "c9")]);

        // Parked (its window shows another graph), the session asks nothing at the polls, nor
        // when the window becomes active, and lets go of its watch on the server, whose health
        // is then checked for no one; shown again, it asks at once, and watches again.
        let watched = |cx: &mut TestAppContext| connection.read_with(cx, |c, _| c.watched());
        assert!(watched(cx), "a shown graph watches its server");
        session.update(cx, |session, cx| session.set_shown(false, cx));
        assert!(!watched(cx), "parked, it holds no watch");
        heads.lock().unwrap().insert("main".into(), "c10".into());
        for _ in 0..2 {
            cx.executor().advance_clock(HEAD_POLL);
            cx.run_until_parked();
        }
        session.update(cx, |session, cx| session.window_activated(cx));
        cx.run_until_parked();
        assert!(probes.lock().unwrap().is_empty(), "parked");
        assert_eq!(head_of(cx, "main").as_deref(), Some("c9"));
        session.update(cx, |session, cx| session.set_shown(true, cx));
        cx.run_until_parked();
        assert_eq!(*probes.lock().unwrap(), vec!["main"]);
        assert_eq!(head_of(cx, "main").as_deref(), Some("c10"));
        assert!(watched(cx), "shown again, it watches again");
    }

    /// The stored-query catalog is read when a view first wants it, not with the graph's first
    /// reads, and once: a server without one has an empty catalog. A stored read goes to the
    /// session's branch and says it expects a read; a stored write goes to the branch it names,
    /// says it expects a write, and is logged like any write, its commit the branch head.
    #[gpui::test]
    async fn stored_queries_are_read_when_wanted_and_run_by_name(cx: &mut TestAppContext) {
        use futures::AsyncReadExt as _;
        use std::sync::atomic::AtomicUsize;
        let sent: Arc<std::sync::Mutex<Vec<(String, Value)>>> = Arc::default();
        let log = sent.clone();
        let serves = Arc::new(AtomicBool::new(true));
        let serving = serves.clone();
        let catalogs = Arc::new(AtomicUsize::new(0));
        let counted = catalogs.clone();
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            let (log, serving) = (log.clone(), serving.clone());
            if path.ends_with("/queries") {
                counted.fetch_add(1, Ordering::SeqCst);
            }
            async move {
                let mut bytes = Vec::new();
                request.into_body().read_to_end(&mut bytes).await.ok();
                let (status, body) = if path.ends_with("/queries") {
                    if serving.load(Ordering::SeqCst) {
                        (
                            200,
                            serde_json::json!({ "queries": [
                            { "name": "find", "tool_name": "find", "mutation": false,
                              "params": [{ "name": "name", "kind": "string", "nullable": false }] },
                            { "name": "add", "tool_name": "add", "mutation": true, "params": [] }
                        ] }),
                        )
                    } else {
                        (404, serde_json::json!({ "error": "not found" }))
                    }
                } else if path.contains("/queries/") {
                    log.lock()
                        .unwrap()
                        .push((path.clone(), serde_json::from_slice(&bytes).unwrap()));
                    if path.ends_with("/find") {
                        (
                            200,
                            serde_json::json!({ "query_name": "find", "target": { "branch": "main" }, "row_count": 0, "rows": [], "graph_commit_id": "c4" }),
                        )
                    } else {
                        (
                            200,
                            serde_json::json!({ "branch": "main", "query_name": "add", "affected_nodes": 1, "affected_edges": 0, "commit": { "graph_commit_id": "c5", "graph_branch": "main", "graph_manifest_version": 5, "kind": "published", "created_at": 5, "parent_commit_id": "c4" } }),
                        )
                    }
                } else {
                    (200, serde_json::json!({ "branches": ["main"] }))
                };
                Ok(Response::builder()
                    .status(status)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                    .body(AsyncBody::from(body.to_string()))
                    .unwrap())
            }
        });
        let server = ServerEntry {
            name: "dev".into(),
            url: "http://127.0.0.1:8787".into(),
            source: ServerSource::Environment,
            known_graphs: Vec::new(),
        };
        let connection = cx.new(|_| Connection::new(server, Some("t".into()), http).unwrap());
        let session = cx
            .new(|cx| GraphSession::new(connection.clone(), "knowledge".into(), "main".into(), cx));
        cx.run_until_parked();
        session.read_with(cx, |session, _| {
            assert!(matches!(session.stored_queries, Loadable::NotLoaded))
        });
        assert_eq!(
            catalogs.load(Ordering::SeqCst),
            0,
            "not read with the graph"
        );
        session.update(cx, |session, cx| session.reload(cx));
        cx.run_until_parked();
        assert_eq!(catalogs.load(Ordering::SeqCst), 0, "nor at a reload");

        // The Queries panel, the palette or a stored query's tab wants it: read once.
        for _ in 0..2 {
            session.update(cx, |session, cx| session.want_stored_queries(cx));
            cx.run_until_parked();
        }
        assert_eq!(catalogs.load(Ordering::SeqCst), 1);
        session.read_with(cx, |session, _| {
            let names: Vec<_> = session
                .stored_queries
                .loaded()
                .unwrap()
                .iter()
                .map(|query| query.name.clone())
                .collect();
            assert_eq!(names, ["find", "add"]);
            assert!(session.stored_query("add").unwrap().mutation);
        });
        // Wanted once, it is read with the graph from then on.
        session.update(cx, |session, cx| session.reload(cx));
        cx.run_until_parked();
        assert_eq!(catalogs.load(Ordering::SeqCst), 2);

        let read = session.update(cx, |session, cx| {
            session.invoke_stored_read("find", Some(serde_json::json!({ "name": "Ada" })), None, cx)
        });
        assert_eq!(read.await.unwrap().graph_commit_id.as_deref(), Some("c4"));
        let write = session.update(cx, |session, cx| {
            session.invoke_stored_write("add", None, "main".into(), None, "Run add".into(), cx)
        });
        assert!(matches!(write.await, WriteOutcome::Committed(_)));
        cx.run_until_parked();
        assert_eq!(
            *sent.lock().unwrap(),
            vec![
                (
                    "/graphs/knowledge/queries/find".to_owned(),
                    serde_json::json!({ "params": { "name": "Ada" }, "branch": "main", "expect_mutation": false })
                ),
                (
                    "/graphs/knowledge/queries/add".to_owned(),
                    serde_json::json!({ "branch": "main", "expect_mutation": true })
                ),
            ]
        );
        session.read_with(cx, |session, _| {
            let last = session.outcomes.last().unwrap();
            assert_eq!(last.description, "Run add");
            assert_eq!(
                last.outcome,
                OutcomeKind::Committed {
                    commit: Some("c5".into())
                }
            );
            assert_eq!(session.head("main"), Some("c5"));
        });

        // A graph the server no longer serves: the catalog failed, it is not empty.
        serves.store(false, Ordering::SeqCst);
        session.update(cx, |session, cx| session.reload_stored_queries(cx));
        cx.run_until_parked();
        session.read_with(cx, |session, _| {
            assert!(matches!(session.stored_queries, Loadable::Failed(_)))
        });
    }

    /// A list of branches reads each branch's newest commit alone, once, and again after a
    /// commit lands on it or a Refresh. A head's read is one commit, never the branch's list.
    #[gpui::test]
    async fn a_branch_head_is_read_alone_and_kept(cx: &mut TestAppContext) {
        let lists: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let seen = lists.clone();
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            let query = request.uri().query().unwrap_or_default().to_owned();
            let seen = seen.clone();
            async move {
                let body = if path.ends_with("/commits") {
                    seen.lock().unwrap().push(query);
                    r#"{"commits":[{"graph_commit_id":"c2","created_at":2,"graph_manifest_version":2,"kind":"published"},{"graph_commit_id":"c1","created_at":1,"graph_manifest_version":1,"kind":"published"}]}"#
                } else {
                    r#"{"branches":["main","feature"]}"#
                };
                Ok(Response::builder()
                    .status(200)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                    .body(AsyncBody::from(body))
                    .unwrap())
            }
        });
        let server = ServerEntry {
            name: "dev".into(),
            url: "http://127.0.0.1:8787".into(),
            source: ServerSource::Environment,
            known_graphs: Vec::new(),
        };
        let connection = cx.new(|_| Connection::new(server, Some("t".into()), http).unwrap());
        let session =
            cx.new(|cx| GraphSession::new(connection, "knowledge".into(), "main".into(), cx));
        cx.run_until_parked();
        let asked = || std::mem::take(&mut *lists.lock().unwrap());
        asked();
        let head = |session: &Entity<GraphSession>, branch: &str, cx: &mut TestAppContext| {
            let read = session.update(cx, |session, cx| session.head_commit(branch, false, cx));
            cx.run_until_parked();
            futures::executor::block_on(read)
                .unwrap()
                .map(|commit| commit.graph_commit_id)
        };

        assert_eq!(head(&session, "main", cx).as_deref(), Some("c2"));
        assert_eq!(asked(), vec!["branch=main&limit=1"], "the head alone");
        assert_eq!(head(&session, "main", cx).as_deref(), Some("c2"));
        assert!(asked().is_empty(), "kept");
        assert_eq!(head(&session, "feature", cx).as_deref(), Some("c2"));
        assert_eq!(asked(), vec!["branch=feature&limit=1"]);
        // A Refresh reads the head again now, though it is held.
        let fresh = session.update(cx, |session, cx| session.head_commit("feature", true, cx));
        cx.run_until_parked();
        futures::executor::block_on(fresh).unwrap();
        assert_eq!(asked(), vec!["branch=feature&limit=1"]);

        // A commit on main: its head is read again.
        session.update(cx, |session, cx| {
            session
                .external_write("main".into(), "w".into(), false, cx)
                .finish(OutcomeKind::Committed {
                    commit: Some("c3".into()),
                });
        });
        cx.run_until_parked();
        asked();
        head(&session, "main", cx);
        assert_eq!(asked(), vec!["branch=main&limit=1"]);
    }

    /// A branch's newest commit is read once for every view that shows it, and again only
    /// after a commit lands on that branch or the branch list is read again.
    #[gpui::test]
    async fn head_commits_are_read_again_only_for_the_branch_a_commit_lands_on(
        cx: &mut TestAppContext,
    ) {
        let lists: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let seen = lists.clone();
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            let query = request.uri().query().unwrap_or_default().to_owned();
            let seen = seen.clone();
            async move {
                let body = if path.ends_with("/commits") {
                    seen.lock().unwrap().push(query);
                    r#"{"commits":[{"graph_commit_id":"c1","created_at":1,"graph_manifest_version":1,"kind":"published"}]}"#
                } else if path.ends_with("/mutate") {
                    r#"{"branch":"main","query_name":"m","affected_nodes":1,"affected_edges":0,"commit":{"graph_commit_id":"c2","graph_branch":"main","graph_manifest_version":2,"kind":"published","created_at":2,"parent_commit_id":"c1"}}"#
                } else {
                    r#"{"branches":["main","feature"]}"#
                };
                Ok(Response::builder()
                    .status(200)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                    .body(AsyncBody::from(body))
                    .unwrap())
            }
        });
        let server = ServerEntry {
            name: "dev".into(),
            url: "http://127.0.0.1:8787".into(),
            source: ServerSource::Environment,
            known_graphs: Vec::new(),
        };
        let connection = cx.new(|_| Connection::new(server, Some("t".into()), http).unwrap());
        let session =
            cx.new(|cx| GraphSession::new(connection, "knowledge".into(), "main".into(), cx));
        // As the window opens: main's head is asked for while the branch list is read, and the
        // branches panel asks again when the branches arrive.
        let _panel = cx.update(|cx| {
            cx.subscribe(&session, |session, event: &SessionEvent, cx| {
                if matches!(event, SessionEvent::BranchesChanged) {
                    session
                        .update(cx, |session, cx| session.head_commit("main", false, cx))
                        .detach();
                }
            })
        });
        let early = session.update(cx, |session, cx| session.head_commit("main", false, cx));
        cx.run_until_parked();
        early.await.unwrap();
        let reads_of = |branch: &str| {
            lists
                .lock()
                .unwrap()
                .iter()
                .filter(|query| query.contains(&format!("branch={branch}")))
                .count()
        };

        assert_eq!(
            reads_of("main"),
            1,
            "the first branch list keeps what was asked for meanwhile"
        );

        // The history and the branches panel ask together: one read.
        let (a, b) = session.update(cx, |session, cx| {
            (
                session.head_commit("main", false, cx),
                session.head_commit("main", false, cx),
            )
        });
        assert!(a.await.unwrap().is_some());
        assert!(b.await.unwrap().is_some());
        let feature = session.update(cx, |session, cx| session.head_commit("feature", false, cx));
        feature.await.unwrap();
        assert_eq!((reads_of("main"), reads_of("feature")), (1, 1));

        // Asked again: nothing is read.
        let again = session.update(cx, |session, cx| session.head_commit("main", false, cx));
        again.await.unwrap();
        assert_eq!(reads_of("main"), 1);

        // Away from main and back: its commits went unwatched meanwhile, so it is read again.
        session.update(cx, |session, cx| session.set_branch("feature".into(), cx));
        session.update(cx, |session, cx| session.set_branch("main".into(), cx));
        cx.run_until_parked();
        let back = session.update(cx, |session, cx| session.head_commit("main", false, cx));
        back.await.unwrap();
        assert_eq!(reads_of("main"), 2);
        let feature_again =
            session.update(cx, |session, cx| session.head_commit("feature", false, cx));
        feature_again.await.unwrap();
        assert_eq!(
            reads_of("feature"),
            2,
            "feature, watched a moment, is read again too"
        );

        // A commit on main: main is read again, feature is not.
        let query = BuiltQuery {
            name: "m".into(),
            text: "query m() { update Person set { title: \"x\" } where @id = \"p\" }".into(),
            params: Default::default(),
            ..Default::default()
        };
        let write = session.update(cx, |session, cx| {
            session.write_on("main", &query, None, "Set title".into(), None, cx)
        });
        write.await;
        cx.run_until_parked();
        for branch in ["main", "feature"] {
            let read = session.update(cx, |session, cx| session.head_commit(branch, false, cx));
            read.await.unwrap();
        }
        assert_eq!((reads_of("main"), reads_of("feature")), (3, 2));

        // Refresh reads it again whatever was kept.
        let fresh = session.update(cx, |session, cx| session.head_commit("feature", true, cx));
        fresh.await.unwrap();
        assert_eq!(reads_of("feature"), 3);

        // The branch list read again unchanged: nothing is read again.
        drop(_panel);
        session.update(cx, |session, cx| session.reload_branches(cx));
        cx.run_until_parked();
        let read = session.update(cx, |session, cx| session.head_commit("feature", false, cx));
        read.await.unwrap();
        assert_eq!(reads_of("feature"), 3);

        // Studio made or deleted feature: its head is read again, main's is not, whatever the
        // branch list shows (deleted and made again under its name, it has another history).
        session.update(cx, |session, cx| {
            session.reload_branches_after("feature".into(), cx)
        });
        cx.run_until_parked();
        for branch in ["main", "feature"] {
            let read = session.update(cx, |session, cx| session.head_commit(branch, false, cx));
            read.await.unwrap();
        }
        assert_eq!((reads_of("main"), reads_of("feature")), (3, 4));
    }

    /// The reads the history, the commit view, the review and the storage inspector ask for
    /// go through the session, so they count as running while they run and stop counting
    /// when dropped.
    #[gpui::test]
    async fn feature_reads_are_counted_while_they_run(cx: &mut TestAppContext) {
        let held = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = held.clone();
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            let held = seen.clone();
            async move {
                let feature = path.ends_with("/changes")
                    || path.ends_with("/healthz")
                    || path.ends_with("/readyz")
                    || path.ends_with("/snapshot")
                    || path == "/graphs";
                if feature {
                    // A read that never answers, held until dropped.
                    held.fetch_add(1, Ordering::SeqCst);
                    futures::future::pending::<()>().await;
                }
                Ok(Response::builder()
                    .status(200)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                    .body(AsyncBody::from(r#"{"branches":["main"]}"#))
                    .unwrap())
            }
        });
        let server = ServerEntry {
            name: "dev".into(),
            url: "http://127.0.0.1:8787".into(),
            source: ServerSource::Environment,
            known_graphs: Vec::new(),
        };
        let connection = cx.new(|_| Connection::new(server, Some("t".into()), http).unwrap());
        let session =
            cx.new(|cx| GraphSession::new(connection, "knowledge".into(), "main".into(), cx));
        cx.run_until_parked();
        // Opening the session reads some of these routes itself.
        let before = session.read_with(cx, |session, _| session.reads_in_flight);
        let held_before = held.load(Ordering::SeqCst);

        let reads = session.update(cx, |session, cx| {
            (
                session.commit_changes("c1", None, 10, cx),
                session.changes("main", Some("beginning".into()), None, 10, cx),
                session.server_state("main", cx),
            )
        });
        cx.run_until_parked();
        assert_eq!(
            held.load(Ordering::SeqCst) - held_before,
            6,
            "two change reads and four server reads"
        );
        session.read_with(cx, |session, _| {
            assert_eq!(session.reads_in_flight, before + 6)
        });
        drop(reads);
        cx.run_until_parked();
        session.read_with(cx, |session, _| assert_eq!(session.reads_in_flight, before));
    }

    /// The command palette's search drops its reads when the text changes; the count must come
    /// down with them. A write whose task is dropped still reaches its outcome and is logged.
    #[gpui::test]
    async fn dropped_reads_are_counted_out_and_dropped_writes_still_finish(
        cx: &mut TestAppContext,
    ) {
        let queries = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mutations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (queries_seen, mutations_seen) = (queries.clone(), mutations.clone());
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            let queries = queries_seen.clone();
            let mutations = mutations_seen.clone();
            async move {
                if path.ends_with("/query") {
                    // A read that never answers, like a slow search.
                    queries.fetch_add(1, Ordering::SeqCst);
                    futures::future::pending::<()>().await;
                }
                let body = if path.ends_with("/mutate") {
                    mutations.fetch_add(1, Ordering::SeqCst);
                    r#"{"error":"refused for the test","code":"bad_request","disposition":{"action":"fix","effects":"none"}}"#
                } else {
                    r#"{"branches":["main"]}"#
                };
                let status = if path.ends_with("/mutate") { 400 } else { 200 };
                Ok(Response::builder()
                    .status(status)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                    .body(AsyncBody::from(body))
                    .unwrap())
            }
        });
        let server = ServerEntry {
            name: "dev".into(),
            url: "http://127.0.0.1:8787".into(),
            source: ServerSource::Environment,
            known_graphs: Vec::new(),
        };
        let connection = cx.new(|_| Connection::new(server, Some("t".into()), http).unwrap());
        let session =
            cx.new(|cx| GraphSession::new(connection, "knowledge".into(), "main".into(), cx));
        cx.run_until_parked();

        let request = || QueryRequest {
            query: "query q() { match { $p: Person } return { $p } }".into(),
            name: Some("q".into()),
            params: None,
            branch: Some("main".into()),
            snapshot: None,
        };
        let reads: Vec<_> = session.update(cx, |session, cx| {
            (0..3)
                .map(|_| session.read_request(request(), cx))
                .collect()
        });
        cx.run_until_parked();
        assert_eq!(queries.load(Ordering::SeqCst), 3);
        session.read_with(cx, |session, _| assert_eq!(session.reads_in_flight, 3));
        drop(reads);
        cx.run_until_parked();
        session.read_with(cx, |session, _| assert_eq!(session.reads_in_flight, 0));

        // A read at a commit is kept under way for whoever asks next, but only while someone
        // waits: dropped by every asker, it stops too, and the next ask reads again.
        let pinned = BuiltQuery {
            name: "q".into(),
            text: "query q() { match { $p: Person } return { $p } }".into(),
            params: serde_json::Map::new(),
            depends: Depends::Unknown,
            seeds: None,
        };
        let asked = queries.load(Ordering::SeqCst);
        let read = session.update(cx, |session, cx| {
            session.read(&pinned, Some("c-old".into()), cx)
        });
        cx.run_until_parked();
        assert_eq!(queries.load(Ordering::SeqCst), asked + 1);
        session.read_with(cx, |session, _| assert_eq!(session.reads_in_flight, 1));
        drop(read);
        cx.run_until_parked();
        session.read_with(cx, |session, _| assert_eq!(session.reads_in_flight, 0));
        let again = session.update(cx, |session, cx| {
            session.read(&pinned, Some("c-old".into()), cx)
        });
        cx.run_until_parked();
        assert_eq!(
            queries.load(Ordering::SeqCst),
            asked + 2,
            "read again, not left waiting on the dropped one"
        );
        drop(again);
        cx.run_until_parked();

        let write = session.update(cx, |session, cx| {
            session.write_request(
                ChangeRequest {
                    query: "query w() { delete Person where @id = \"p\" }".into(),
                    name: Some("w".into()),
                    params: None,
                    branch: Some("main".into()),
                },
                None,
                "Delete p".into(),
                cx,
            )
        });
        drop(write);
        cx.run_until_parked();
        assert_eq!(mutations.load(Ordering::SeqCst), 1);
        session.read_with(cx, |session, _| {
            assert_eq!(session.writes_in_flight, 0);
            assert_eq!(session.outcomes.len(), 1);
            assert!(matches!(
                session.outcomes[0].outcome,
                OutcomeKind::Refused { .. }
            ));
        });
    }

    /// A write that answers after the watch saw a newer commit on top of it does not leave the
    /// head on the older one: the head is read again at once.
    /// A carry leaves out the commits whose answer is no longer held, so no walk goes back for
    /// them again; the commit carried to stays.
    /// A carry looks only for commits the walk back from its commit can meet: the commit the
    /// log started after carries past a kept one whose answer changed, and one newer than the
    /// commit asked for, or one the log never met, is not looked for.
    #[test]
    fn a_carry_forgets_commits_whose_answer_is_gone() {
        use crate::carry::tests::{block, page};
        use serde_json::json;

        let mut kept = vec!["c1".to_owned(), "c2".to_owned(), "c3".to_owned()];
        let found: Option<()> =
            carry_back(None, "c3", &mut kept, |commit| commit == "c2", |_, _| None);
        assert!(found.is_none());
        assert_eq!(kept, ["c2", "c3"]);

        let mut fold = FeedFold::new(Arc::new(BTreeSet::from(["Person".to_owned()])));
        fold.page(&page(
            json!([
                block("c2", "c1", json!([])),
                block("c3", "c2", json!([])),
                block("c4", "c3", json!([])),
            ]),
            true,
        ));
        let mut log = ChangeLog::default();
        for (commit, logged) in fold.complete() {
            log.record(commit, logged);
        }
        let mut asked = Vec::new();
        let mut carry = |to: &str, kept: &[&str]| {
            let mut kept: Vec<String> = kept.iter().map(|commit| commit.to_string()).collect();
            carry_back(
                Some(&log),
                to,
                &mut kept,
                |_| true,
                |start, _| {
                    asked.push(start.to_owned());
                    (start == "c1").then(|| start.to_owned())
                },
            )
        };
        assert_eq!(carry("c4", &["c1", "c3"]).as_deref(), Some("c1"));
        assert_eq!(carry("c3", &["c4", "c9"]), None);
        assert_eq!(asked, ["c3", "c1"]);
    }

    /// A page of a commit's changes is read once, for every view that asks: a commit never
    /// changes. Every view is handed the one page read, not a copy of its changes.
    #[gpui::test]
    async fn a_commits_changes_are_read_once_and_shared(cx: &mut TestAppContext) {
        let pages = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = pages.clone();
        let http = FakeHttpClient::create(move |request| {
            let counted = counted.clone();
            async move {
                let body = if request.uri().path().ends_with("/changes") {
                    counted.fetch_add(1, Ordering::SeqCst);
                    r#"{"cause":{"graph_commit_id":"c1","authored_branch":"main","authored_at":1},"changes":[{"kind":"node","type":{"id":"t1","name":"Person"},"id":"p1","op":"update","before":{"properties":{"name":"Ada"}},"after":{"properties":{"name":"Grace"}}}]}"#
                } else {
                    r#"{"branches":["main"]}"#
                };
                Ok(Response::builder()
                    .status(200)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                    .body(AsyncBody::from(body))
                    .unwrap())
            }
        });
        let server = ServerEntry {
            name: "dev".into(),
            url: "http://127.0.0.1:8787".into(),
            source: ServerSource::Environment,
            known_graphs: Vec::new(),
        };
        let connection = cx.new(|_| Connection::new(server, Some("t".into()), http).unwrap());
        let session =
            cx.new(|cx| GraphSession::new(connection, "knowledge".into(), "main".into(), cx));
        cx.run_until_parked();
        let at_start = pages.load(Ordering::SeqCst);
        let (panel, tab) = session.update(cx, |session, cx| {
            (
                session.commit_changes("c1", None, 500, cx),
                session.commit_changes("c1", None, 500, cx),
            )
        });
        let (panel, tab) = (panel.await.unwrap(), tab.await.unwrap());
        let again = session.update(cx, |session, cx| {
            session.commit_changes("c1", None, 500, cx)
        });
        let again = again.await.unwrap();
        // The page came back whole: a larger ask (the commit view beside a History row) takes
        // it too.
        let larger = session.update(cx, |session, cx| {
            session.commit_changes("c1", None, 5_000, cx)
        });
        let larger = larger.await.unwrap();
        assert_eq!(pages.load(Ordering::SeqCst) - at_start, 1);
        assert_eq!(panel.changes.len(), 1);
        // The read shared while under way, the answer kept, and the whole page answering a
        // larger ask all hold the changes decoded once.
        for (asked, page) in [("tab", &tab), ("again", &again), ("larger", &larger)] {
            assert!(
                Arc::ptr_eq(&panel.changes, &page.changes),
                "{asked} holds a copy"
            );
        }
    }

    /// A branch Studio deleted takes its unsaved values with it, and so does one deleted
    /// elsewhere (the CLI, an agent), once the branch list no longer holds it; a branch still
    /// listed keeps them.
    #[gpui::test]
    async fn a_deleted_branch_takes_its_drafts(cx: &mut TestAppContext) {
        use crate::test_server::followed_session;
        use serde_json::json;

        const SCHEMA: &str = "node Person {\n  slug: String @key\n  name: String?\n}\n";
        let (session, served) = followed_session(SCHEMA, |_| json!([]), cx);
        served.lock().unwrap().branches = ["main", "feature", "kept", "elsewhere"]
            .map(str::to_owned)
            .into();
        cx.run_until_parked();
        let key = DraftKey {
            type_name: "Person".into(),
            id: "p1".into(),
            property: "name".into(),
        };
        let entry = DraftEntry {
            old: json!("Ada"),
            text: "Grace".into(),
        };
        for branch in ["feature", "kept", "elsewhere"] {
            session.update(cx, |session, cx| {
                session.set_draft_on(branch, key.clone(), entry.clone(), cx)
            });
        }
        served.lock().unwrap().branches.retain(|b| b != "feature");
        let delete = session.update(cx, |session, cx| {
            session.delete_branch("feature".into(), cx)
        });
        assert!(matches!(delete.await, WriteOutcome::Committed(_)));
        cx.run_until_parked();
        let drafts = |branch: &str, cx: &mut TestAppContext| {
            session.read_with(cx, |session, _| session.drafts_on(branch).count())
        };
        assert_eq!(drafts("feature", cx), 0);
        assert_eq!(drafts("kept", cx), 1);

        // A save settling after its branch was deleted keeps no draft there; once a branch of
        // that name is listed again, one does.
        let settle = |cx: &mut TestAppContext| {
            session.update(cx, |session, cx| {
                session.settle_draft_on("feature", key.clone(), None, Some(entry.clone()), cx)
            })
        };
        settle(cx);
        assert_eq!(drafts("feature", cx), 0);
        served.lock().unwrap().branches.push("feature".into());
        session.update(cx, |session, cx| session.reload_branches(cx));
        cx.run_until_parked();
        settle(cx);
        assert_eq!(drafts("feature", cx), 1);

        // Deleted by the CLI: the poll's branch list no longer holds it.
        served.lock().unwrap().branches.retain(|b| b != "elsewhere");
        session.update(cx, |session, cx| session.check_heads(cx));
        cx.run_until_parked();
        assert_eq!(drafts("elsewhere", cx), 0);
        assert_eq!(drafts("kept", cx), 1);
    }

    /// What the credential may do is read for the session's branch when it opens and moves
    /// there, once per branch, and again after the server refuses a write with 403. A 403 on
    /// one graph says nothing of another on the server: a session of the other keeps its
    /// answer, and asks nothing again.
    #[gpui::test]
    async fn access_is_read_per_branch_and_again_after_a_refusal(cx: &mut TestAppContext) {
        use crate::access::{AccessScope, Allowed};
        use graph_client::api::AccessAction;
        use serde_json::json;
        use std::sync::atomic::AtomicUsize;

        let asked: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let notes_asked = Arc::new(AtomicUsize::new(0));
        let (seen, notes_seen) = (asked.clone(), notes_asked.clone());
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            let query = request.uri().query().unwrap_or_default().to_owned();
            if path.ends_with("/graphs/notes/access") {
                notes_seen.fetch_add(1, Ordering::SeqCst);
            } else if path.ends_with("/access") {
                seen.lock().unwrap().push(query.clone());
            }
            let create = request.method().as_str() == "POST" && path.ends_with("/branches");
            async move {
                let (status, body) = if path.ends_with("/access") {
                    // Every branch reads; only fix takes changes.
                    let fix = query.contains("branch=fix");
                    let actions = if fix {
                        json!(["read", "change"])
                    } else {
                        json!(["read"])
                    };
                    (
                        200,
                        json!({ "graph_id": "knowledge", "actor_id": "act-ada", "graph_wide": ["read"], "actions": actions }),
                    )
                } else if create {
                    (
                        403,
                        json!({
                            "error": "policy denied",
                            "code": "forbidden",
                            "disposition": { "action": "refresh", "effects": "none" },
                        }),
                    )
                } else {
                    (200, json!({ "branches": ["main", "fix"] }))
                };
                Ok(Response::builder()
                    .status(status)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                    .body(AsyncBody::from(body.to_string()))
                    .unwrap())
            }
        });
        let server = ServerEntry {
            name: "dev".into(),
            url: "http://127.0.0.1:8787".into(),
            source: ServerSource::Environment,
            known_graphs: Vec::new(),
        };
        let connection = cx.new(|_| Connection::new(server, Some("t".into()), http).unwrap());
        let session = cx
            .new(|cx| GraphSession::new(connection.clone(), "knowledge".into(), "main".into(), cx));
        // Another graph of the server, on the same connection, in another window.
        let notes = cx.new(|cx| GraphSession::new(connection, "notes".into(), "main".into(), cx));
        cx.run_until_parked();
        let taken = || std::mem::take(&mut *asked.lock().unwrap());
        assert_eq!(taken(), ["branch=main"]);
        assert_eq!(notes_asked.load(Ordering::SeqCst), 1);
        session.read_with(cx, |session, _| {
            assert_eq!(session.allows_here(AccessAction::Change), Allowed::No);
            assert_eq!(session.allows_here(AccessAction::Read), Allowed::Yes);
            assert_eq!(session.actor(), Some("act-ada"));
        });

        session.update(cx, |session, cx| session.set_branch("fix".into(), cx));
        cx.run_until_parked();
        session.update(cx, |session, cx| session.set_branch("main".into(), cx));
        cx.run_until_parked();
        assert_eq!(taken(), ["branch=fix"], "main's answer is kept");
        session.read_with(cx, |session, _| {
            let fix = AccessScope::branch("fix");
            assert_eq!(session.allows(AccessAction::Change, &fix), Allowed::Yes);
        });

        let refused = session.update(cx, |session, cx| {
            session.create_branch("x".into(), BranchStart::Head("main".into()), cx)
        });
        assert!(matches!(refused.await, WriteOutcome::Refused(_)));
        cx.run_until_parked();
        assert_eq!(taken(), ["branch=main"], "read again after the 403");
        session.read_with(cx, |session, _| {
            let fix = AccessScope::branch("fix");
            assert_eq!(
                session.allows(AccessAction::Change, &fix),
                Allowed::Unknown,
                "fix's answer was forgotten"
            );
        });
        // notes' answer stands: asked for again, it is the one held.
        notes.update(cx, |notes, cx| {
            notes.read_access(AccessScope::branch("main"), cx)
        });
        cx.run_until_parked();
        assert_eq!(notes_asked.load(Ordering::SeqCst), 1, "notes asked nothing");
        notes.read_with(cx, |notes, _| {
            assert_eq!(notes.allows_here(AccessAction::Read), Allowed::Yes);
        });
    }

    /// A new token is a new credential: what the old one was allowed is read again at once,
    /// with no write to bring it about, an access answer still out from before lands nowhere,
    /// and no answer kept from the old token is given again, since the new one may see less:
    /// the type counts, the stored-query catalog and the branch list are read again, and none
    /// of the old token's shows meanwhile. A 403 alone reads what the credential may do again
    /// and keeps the answers. A parked graph forgets at once and reads only when shown again.
    #[gpui::test]
    async fn a_new_token_reads_access_and_every_answer_again(cx: &mut TestAppContext) {
        use futures::AsyncReadExt as _;
        use serde_json::json;
        use std::sync::atomic::AtomicUsize;

        use crate::access::Allowed;
        use graph_client::api::AccessAction;

        // The next access read waits here, once a gate is set; reads other than the head probe
        // are counted, and the graph's counts, catalog and branch list asked for are listed.
        let gate: Arc<std::sync::Mutex<Option<oneshot::Receiver<()>>>> = Arc::default();
        let reads = Arc::new(AtomicUsize::new(0));
        let accesses = Arc::new(AtomicUsize::new(0));
        let shown: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let (held, counted, asked, listed) =
            (gate.clone(), reads.clone(), accesses.clone(), shown.clone());
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            if let Some(read) = ["/snapshot", "/queries", "/branches"]
                .iter()
                .find(|read| path.ends_with(*read))
            {
                listed.lock().unwrap().push((*read).to_owned());
            }
            let token = request
                .headers()
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_owned();
            if path.ends_with("/access") {
                asked.fetch_add(1, Ordering::SeqCst);
            }
            let wait = path
                .ends_with("/access")
                .then(|| held.lock().unwrap().take())
                .flatten();
            let counted = counted.clone();
            async move {
                let mut bytes = Vec::new();
                request.into_body().read_to_end(&mut bytes).await.ok();
                if let Some(wait) = wait {
                    wait.await.ok();
                }
                let (status, body) = if path.ends_with("/access") {
                    // The read-only token reads; the other may change too.
                    let actions = if token == "Bearer rw" {
                        json!(["read", "change"])
                    } else {
                        json!(["read"])
                    };
                    let body = json!({ "graph_id": "knowledge", "actor_id": "act-ada",
                        "graph_wide": ["read"], "branch": "main", "actions": actions });
                    (200, body)
                } else if path.ends_with("/schema") {
                    (
                        200,
                        json!({ "schema_source": "node Person {\n  slug: String @key\n}\n" }),
                    )
                } else if path.ends_with("/query") {
                    let asked: Value = serde_json::from_slice(&bytes).unwrap();
                    if asked["name"] != "studio_head" {
                        counted.fetch_add(1, Ordering::SeqCst);
                    }
                    let body = json!({ "query_name": asked["name"], "target": { "branch": "main" },
                        "row_count": 0, "columns": [], "rows": [], "graph_commit_id": "c1" });
                    (200, body)
                } else if path.ends_with("/mutate") {
                    (
                        403,
                        json!({
                            "error": "policy denied",
                            "code": "forbidden",
                            "disposition": { "action": "refresh", "effects": "none" },
                        }),
                    )
                } else if path.ends_with("/snapshot") {
                    let body = json!({ "graph_branch": "main", "graph_manifest_version": 3,
                        "internal_schema_version": 16, "datasets": [{ "entity_kind": "node",
                        "type_name": "Person", "dataset_path": "nodes/1",
                        "published_dataset_version": 2, "entity_count": 7 }] });
                    (200, body)
                } else if path.ends_with("/queries") {
                    (200, json!({ "queries": [] }))
                } else {
                    (200, json!({ "branches": ["main"] }))
                };
                Ok(Response::builder()
                    .status(status)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                    .body(AsyncBody::from(body.to_string()))
                    .unwrap())
            }
        });
        let server = ServerEntry {
            name: "dev".into(),
            url: "http://127.0.0.1:8787".into(),
            source: ServerSource::Environment,
            known_graphs: Vec::new(),
        };
        // The access read the session opens with, under the read-only token, is held.
        let (release, wait) = oneshot::channel::<()>();
        *gate.lock().unwrap() = Some(wait);
        let connection = cx.new(|_| Connection::new(server, Some("ro".into()), http).unwrap());
        let session = cx.new(|cx| {
            let mut session =
                GraphSession::new(connection.clone(), "knowledge".into(), "main".into(), cx);
            session.want_stored_queries(cx);
            session
        });
        cx.run_until_parked();
        let change = |cx: &mut TestAppContext| {
            session.read_with(cx, |session, _| session.allows_here(AccessAction::Change))
        };
        assert_eq!(change(cx), Allowed::Unknown, "still out");

        // `omnigraph login` gave a write-capable token: read again at once, under it, and the
        // read-only answer still out lands nowhere.
        connection.update(cx, |connection, cx| {
            connection.set_token(Some("rw".into()), cx)
        });
        cx.run_until_parked();
        assert_eq!(change(cx), Allowed::Yes);
        release.send(()).ok();
        cx.run_until_parked();
        assert_eq!(
            change(cx),
            Allowed::Yes,
            "the old token's answer came too late"
        );

        // An answer kept at a commit is given again without asking, until the token changes.
        let query = BuiltQuery {
            name: "people".into(),
            text: "query people() { match { $p: Person } return { $p.slug } }".into(),
            ..Default::default()
        };
        let read_at_c1 = |cx: &mut TestAppContext| {
            let read = session.update(cx, |session, cx| {
                session.read(&query, Some("c1".into()), cx)
            });
            cx.run_until_parked();
            futures::executor::block_on(read).unwrap();
            reads.swap(0, Ordering::SeqCst)
        };
        assert_eq!(read_at_c1(cx), 1);
        assert_eq!(read_at_c1(cx), 0, "kept");
        session.read_with(cx, |session, _| {
            assert_eq!(session.count_of(EntityKind::Node, "Person"), Some(7));
            assert!(matches!(session.branches, Loadable::Loaded(_)));
            assert!(matches!(session.stored_queries, Loadable::Loaded(_)));
        });
        shown.lock().unwrap().clear();
        connection.update(cx, |connection, cx| {
            connection.set_token(Some("ro".into()), cx)
        });
        // Until the new token's answers land, none of the old token's shows.
        session.read_with(cx, |session, _| {
            assert_eq!(session.count_of(EntityKind::Node, "Person"), None);
            assert!(session.branches.is_loading());
            assert!(session.stored_queries.is_loading());
        });
        cx.run_until_parked();
        let mut asked_again = shown.lock().unwrap().clone();
        asked_again.sort();
        assert_eq!(asked_again, ["/branches", "/queries", "/snapshot"]);
        session.read_with(cx, |session, _| {
            assert_eq!(session.count_of(EntityKind::Node, "Person"), Some(7));
            assert!(matches!(session.branches, Loadable::Loaded(_)));
        });
        assert_eq!(change(cx), Allowed::No, "the narrower token's own answer");
        assert_eq!(read_at_c1(cx), 1, "asked again with the new token");

        // A 403 reads what the credential may do again, and keeps what was read.
        accesses.store(0, Ordering::SeqCst);
        let refused = session.update(cx, |session, cx| {
            session.write_on("main", &query, None, "Write".into(), None, cx)
        });
        assert!(matches!(refused.await, WriteOutcome::Refused(_)));
        cx.run_until_parked();
        assert_eq!(
            accesses.load(Ordering::SeqCst),
            1,
            "read again after the 403"
        );
        assert_eq!(change(cx), Allowed::No);
        assert_eq!(read_at_c1(cx), 0, "kept through a 403");

        // A graph its window parked asks the server nothing for a new token: the old token's
        // answers go at once, and the new token's are read when the graph is shown again. Its
        // views hear of the branch list then too, not while parked, since they read on it.
        session.update(cx, |session, cx| session.set_shown(false, cx));
        cx.run_until_parked();
        shown.lock().unwrap().clear();
        accesses.store(0, Ordering::SeqCst);
        let heard: Rc<std::cell::RefCell<Vec<SessionEvent>>> = Rc::default();
        let hears = heard.clone();
        let _heard = cx.update(|cx| {
            cx.subscribe(&session, move |_, event: &SessionEvent, _| {
                if matches!(
                    event,
                    SessionEvent::BranchesChanged | SessionEvent::CredentialChanged
                ) {
                    hears.borrow_mut().push(event.clone());
                }
            })
        });
        connection.update(cx, |connection, cx| {
            connection.set_token(Some("rw".into()), cx)
        });
        cx.run_until_parked();
        assert!(
            shown.lock().unwrap().is_empty(),
            "a parked graph read {:?}",
            shown.lock().unwrap()
        );
        assert_eq!(accesses.load(Ordering::SeqCst), 0, "nor what it may do");
        assert!(
            heard.take().is_empty(),
            "nothing a view reads on while parked"
        );
        session.read_with(cx, |session, _| {
            assert_eq!(session.count_of(EntityKind::Node, "Person"), None);
            assert!(session.branches.is_loading());
            assert!(session.stored_queries.is_loading());
        });
        session.update(cx, |session, cx| session.set_shown(true, cx));
        cx.run_until_parked();
        let mut asked_again = shown.lock().unwrap().clone();
        asked_again.sort();
        assert_eq!(asked_again, ["/branches", "/queries", "/snapshot"]);
        assert_eq!(accesses.load(Ordering::SeqCst), 1);
        assert_eq!(change(cx), Allowed::Yes, "the new token's own answer");
        let heard = heard.take();
        assert_eq!(
            heard.first(),
            Some(&SessionEvent::BranchesChanged),
            "shown, the branch list read again: {heard:?}"
        );
        assert!(heard.contains(&SessionEvent::CredentialChanged));
        session.read_with(cx, |session, _| {
            assert_eq!(session.count_of(EntityKind::Node, "Person"), Some(7));
        });
    }

    /// A graph shown while its connection's token is still on its way from the CLI sends
    /// nothing until the token is there, then reads it all, the schema included, with no
    /// Refresh. Before, its first reads went out with no token and were refused (401), and a new
    /// token read the counts, the catalog and the branch list again but not the schema, so a
    /// window a start reopened stayed empty until Refresh. A schema a 401 refused is read again
    /// when the token changes, as every read a 401 could have refused is.
    #[gpui::test]
    async fn the_first_reads_wait_for_the_token_and_the_schema_loads(cx: &mut TestAppContext) {
        use futures::AsyncReadExt as _;
        use serde_json::json;

        use crate::cli_tokens::{Token, TokenSource};

        /// A token source that answers when the test gives it its token.
        struct Gated(std::sync::Mutex<Option<oneshot::Receiver<&'static str>>>);
        impl TokenSource for Gated {
            fn token(
                &self,
                _: Arc<AtomicBool>,
            ) -> futures::future::BoxFuture<'static, Result<Token, String>> {
                let gate = self.0.lock().unwrap().take();
                async move {
                    match gate {
                        Some(gate) => gate.await.map(Token::new).map_err(|_| "gone".into()),
                        None => Ok(Token::new("good")),
                    }
                }
                .boxed()
            }
        }

        // Answers the token `good` alone; every graph read is listed with its token.
        let asked: Arc<std::sync::Mutex<Vec<(String, String)>>> = Arc::default();
        let log = asked.clone();
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            let token = request
                .headers()
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_owned();
            let log = log.clone();
            async move {
                let mut bytes = Vec::new();
                request.into_body().read_to_end(&mut bytes).await.ok();
                let read = path.rsplit('/').next().unwrap_or_default().to_owned();
                if path.contains("/graphs/knowledge") {
                    log.lock().unwrap().push((read.clone(), token.clone()));
                }
                let (status, body) = if read == "healthz" {
                    (
                        200,
                        json!({ "status": "ok", "version": "0.14.0", "internal_schema_version": 16 }),
                    )
                } else if token != "Bearer good" {
                    (
                        401,
                        json!({ "error": "unauthorized", "code": "unauthorized" }),
                    )
                } else {
                    let body = match read.as_str() {
                        "graphs" => json!({ "graphs": [] }),
                        "schema" => {
                            json!({ "schema_source": "node Person {\n  slug: String @key\n}\n" })
                        }
                        "snapshot" => json!({ "graph_branch": "main", "graph_manifest_version": 3,
                            "internal_schema_version": 16, "datasets": [] }),
                        "access" => json!({ "graph_id": "knowledge", "actor_id": "act-ada",
                            "graph_wide": ["read"], "branch": "main", "actions": ["read"] }),
                        "branches" => json!({ "branches": ["main"] }),
                        _ => json!({ "query_name": "studio_head", "target": { "branch": "main" },
                            "row_count": 0, "columns": [], "rows": [], "graph_commit_id": "c1" }),
                    };
                    (200, body)
                };
                let response = Response::builder()
                    .status(status)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT);
                let response = if status == 401 {
                    response.header("www-authenticate", "Bearer")
                } else {
                    response
                };
                Ok(response.body(AsyncBody::from(body.to_string())).unwrap())
            }
        });
        let server = ServerEntry {
            name: "personal".into(),
            url: "https://graph.test".into(),
            source: ServerSource::Cli,
            known_graphs: Vec::new(),
        };
        let (give, gate) = oneshot::channel();
        let source: Arc<dyn TokenSource> = Arc::new(Gated(std::sync::Mutex::new(Some(gate))));
        let connection = cx.new(|_| {
            Connection::new(server.clone(), None, http.clone())
                .unwrap()
                .with_token_source(Some(source))
        });
        // As a start reopens a window: the server connects, then its graph is shown at once.
        connection.update(cx, |connection, cx| connection.connect(cx));
        let session =
            cx.update(|cx| open_session(connection.clone(), "knowledge".into(), "main".into(), cx));
        cx.run_until_parked();
        assert!(
            asked.lock().unwrap().is_empty(),
            "nothing sent without the token: {:?}",
            asked.lock()
        );
        assert!(matches!(
            session.read_with(cx, |session, _| session.schema.clone()),
            Loadable::NotLoaded
        ));

        give.send("good").ok();
        cx.run_until_parked();
        let sent = std::mem::take(&mut *asked.lock().unwrap());
        assert!(
            sent.iter().all(|(_, token)| token == "Bearer good"),
            "every read carried the token: {sent:?}"
        );
        for read in ["schema", "snapshot", "branches", "access"] {
            assert!(
                sent.iter().any(|(sent, _)| sent == read),
                "{read}: {sent:?}"
            );
        }
        session.read_with(cx, |session, _| {
            assert!(session.schema_model().is_some(), "with no Refresh");
            assert!(matches!(session.branches, Loadable::Loaded(_)));
        });

        // A token the server refuses leaves the schema unread; the next token reads it again.
        let refused = cx.new(|_| Connection::new(server, Some("stale".into()), http).unwrap());
        let session =
            cx.update(|cx| open_session(refused.clone(), "knowledge".into(), "main".into(), cx));
        cx.run_until_parked();
        assert!(matches!(
            session.read_with(cx, |session, _| session.schema.clone()),
            Loadable::Failed(_)
        ));
        asked.lock().unwrap().clear();
        refused.update(cx, |connection, cx| {
            connection.set_token(Some("good".into()), cx)
        });
        cx.run_until_parked();
        let sent = std::mem::take(&mut *asked.lock().unwrap());
        assert!(sent.iter().any(|(sent, _)| sent == "schema"), "{sent:?}");
        session.read_with(cx, |session, _| {
            assert!(session.schema_model().is_some(), "with no Refresh")
        });
    }

    /// A branch starts at a branch's head or at a commit, and its request names the one, never
    /// both. The receipt's commit is the new branch's head; a commit start's outcome names it.
    #[gpui::test]
    async fn a_branch_starts_at_a_head_or_at_a_commit_never_both(cx: &mut TestAppContext) {
        use futures::AsyncReadExt as _;
        use serde_json::json;

        let sent = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let seen = sent.clone();
        let http = FakeHttpClient::create(move |request| {
            let create =
                request.method().as_str() == "POST" && request.uri().path().ends_with("/branches");
            let seen = seen.clone();
            async move {
                let mut bytes = Vec::new();
                request.into_body().read_to_end(&mut bytes).await.ok();
                let body = if create {
                    let asked: Value = serde_json::from_slice(&bytes).unwrap();
                    seen.lock().unwrap().push(asked.clone());
                    // A head start begins at main's head, a commit start at its commit.
                    let at = asked.get("from_commit").cloned().unwrap_or(json!("m9"));
                    json!({ "uri": "u", "from": "main", "name": asked["name"], "from_commit_id": at })
                } else {
                    json!({ "branches": ["main"] })
                };
                Ok(Response::builder()
                    .status(200)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                    .body(AsyncBody::from(body.to_string()))
                    .unwrap())
            }
        });
        let server = ServerEntry {
            name: "dev".into(),
            url: "http://127.0.0.1:8787".into(),
            source: ServerSource::Environment,
            known_graphs: Vec::new(),
        };
        let connection = cx.new(|_| Connection::new(server, Some("t".into()), http).unwrap());
        let session =
            cx.new(|cx| GraphSession::new(connection, "knowledge".into(), "main".into(), cx));
        cx.run_until_parked();
        let head = session.update(cx, |session, cx| {
            session.create_branch("fix".into(), BranchStart::Head("main".into()), cx)
        });
        assert!(matches!(head.await, WriteOutcome::Committed(_)));
        let at = session.update(cx, |session, cx| {
            session.create_branch("then".into(), BranchStart::Commit("c7".into()), cx)
        });
        assert!(matches!(
            at.await,
            WriteOutcome::Committed(created) if created.from_commit_id == "c7"
        ));
        cx.run_until_parked();
        assert_eq!(
            *sent.lock().unwrap(),
            [
                json!({ "name": "fix", "from": "main" }),
                json!({ "name": "then", "from_commit": "c7" }),
            ]
        );
        session.read_with(cx, |session, _| {
            assert_eq!(session.head("fix"), Some("m9"));
            assert_eq!(session.head("then"), Some("c7"));
            let described: Vec<&str> = session
                .outcomes
                .iter()
                .map(|outcome| outcome.description.as_str())
                .collect();
            assert_eq!(described, ["Create branch fix", "Create branch then at c7"]);
        });
    }

    #[gpui::test]
    async fn a_late_answer_of_studios_own_write_does_not_leave_an_older_head(
        cx: &mut TestAppContext,
    ) {
        use crate::test_server::followed_session;
        use serde_json::json;

        const SCHEMA: &str = "node Person {\n  slug: String @key\n}\n";
        let (session, served) = followed_session(SCHEMA, |_| json!([]), cx);
        cx.run_until_parked();
        let _watch = session.update(cx, |session, _| watching(session, &["Person"], &[]));
        // Studio's write commits m; another writer commits n on top; the watch sees n first.
        served.lock().unwrap().commit("m", Vec::new());
        served.lock().unwrap().commit("n", Vec::new());
        session.update(cx, |session, cx| {
            session.saw_head("main".into(), "n".into(), false, None, cx)
        });
        cx.run_until_parked();
        let moves = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let heard = moves.clone();
        let _moves = cx.update(|cx| {
            cx.subscribe(&session, move |_, event: &SessionEvent, _| {
                if let SessionEvent::HeadMoved { head, .. } = event {
                    heard.lock().unwrap().push(head.clone());
                }
            })
        });
        session.update(cx, |session, cx| {
            session.committed("main".into(), "m".into(), Some("c1".into()), cx)
        });
        cx.run_until_parked();
        assert_eq!(
            session.read_with(cx, |session, _| session.head("main").map(str::to_owned)),
            Some("n".into())
        );
        // Views never heard of a move back to m: one pinned at n would find m unrelated and
        // offer newer data that never clears.
        assert!(
            moves.lock().unwrap().is_empty(),
            "{:?}",
            moves.lock().unwrap()
        );
    }

    /// One of Studio's own commits that names no parent (an agent's write) is placed by a head
    /// read; found at the head, it is not another writer's commit, and lands once.
    #[gpui::test]
    async fn an_own_commit_a_head_read_places_lands_once(cx: &mut TestAppContext) {
        use crate::test_server::followed_session;
        use serde_json::json;

        const SCHEMA: &str = "node Person {\n  slug: String @key\n}\n";
        let (session, served) = followed_session(SCHEMA, |_| json!([]), cx);
        cx.run_until_parked();
        let events: Rc<std::cell::RefCell<Vec<SessionEvent>>> = Rc::default();
        let seen = events.clone();
        let _events = cx.update(|cx| {
            cx.subscribe(&session, move |_, event: &SessionEvent, _| {
                if matches!(
                    event,
                    SessionEvent::Committed { .. } | SessionEvent::HeadMoved { .. }
                ) {
                    seen.borrow_mut().push(event.clone());
                }
            })
        });
        served.lock().unwrap().commit("c2", Vec::new());
        session.update(cx, |session, cx| {
            session
                .external_write("main".into(), "Agent write".into(), false, cx)
                .finish(OutcomeKind::Committed {
                    commit: Some("c2".into()),
                });
        });
        cx.run_until_parked();
        assert_eq!(
            events.take(),
            vec![
                SessionEvent::Committed {
                    branch: "main".into()
                },
                SessionEvent::HeadMoved {
                    branch: "main".into(),
                    head: "c2".into()
                },
            ]
        );
        assert_eq!(
            session.read_with(cx, |session, _| session.head("main").map(str::to_owned)),
            Some("c2".into())
        );
    }

    /// Another writer renames the type the head probe matches: the server refuses the probe,
    /// and the session reads the schema again, takes it, and sees the head under it. With no
    /// view reading the branch, a head move still compares the schema, as a feed read would,
    /// even when Studio's own commit on top of it is the head the read finds.
    #[gpui::test]
    async fn a_schema_changed_elsewhere_is_seen_with_no_view_reading_the_branch(
        cx: &mut TestAppContext,
    ) {
        use crate::test_server::followed_session;
        use serde_json::json;

        const SCHEMA: &str = "node Person {\n  slug: String @key\n}\n";
        let (session, served) = followed_session(SCHEMA, |_| json!([]), cx);
        cx.run_until_parked();
        let moved = Rc::new(std::cell::Cell::new(0));
        let heard = moved.clone();
        let _events = cx.update(|cx| {
            cx.subscribe(&session, move |_, event: &SessionEvent, _| {
                if *event == SessionEvent::SchemaMoved {
                    heard.set(heard.get() + 1);
                }
            })
        });
        let held = |cx: &mut TestAppContext| {
            session.read_with(cx, |session, _| {
                let schema = session.schema_model().unwrap().source.to_string();
                (schema, session.head("main").map(str::to_owned))
            })
        };

        let renamed = "node Human {\n  slug: String @key\n}\n";
        served.lock().unwrap().schema = renamed.into();
        served.lock().unwrap().commit("c2", Vec::new());
        session.update(cx, |session, cx| session.check_heads(cx));
        cx.run_until_parked();
        assert_eq!(held(cx), (renamed.to_owned(), Some("c2".into())));
        assert_eq!(moved.get(), 1);

        // A type added, the probe unchanged: the head move compares the schema.
        let added = format!("{renamed}node Note {{\n  slug: String @key\n}}\n");
        served.lock().unwrap().schema = added.clone();
        served.lock().unwrap().commit("c3", Vec::new());
        session.update(cx, |session, cx| session.check_heads(cx));
        cx.run_until_parked();
        assert_eq!(held(cx), (added.clone(), Some("c3".into())));
        assert_eq!(moved.get(), 2);

        // Another writer's move seen while a change read was out, then Studio's own commit on
        // top of it: the read after them finds Studio's commit at the head, and compares the
        // schema all the same.
        let tagged = format!("{added}node Tag {{\n  slug: String @key\n}}\n");
        served.lock().unwrap().schema = tagged.clone();
        served.lock().unwrap().commit("c4", Vec::new());
        served.lock().unwrap().commit("c5", Vec::new());
        session.update(cx, |session, cx| {
            session.change_reads.insert("main".into(), Task::ready(()));
            session.saw_head("main".into(), "c4".into(), false, None, cx);
            session.saw_head("main".into(), "c5".into(), true, Some("c4"), cx);
            session.change_reads.remove("main");
            assert!(session.change_reads_again.remove("main"));
            session.read_changes("main", cx);
        });
        cx.run_until_parked();
        assert_eq!(held(cx), (tagged, Some("c5".into())));
        assert_eq!(moved.get(), 3);
    }

    /// Rechecks of the schema go one at a time: reasons met before a read leaves are answered
    /// by it, and one met after it left asks once more once it is back. Studio's own commit to
    /// a branch no view reads changes no schema, and reads none.
    #[gpui::test]
    async fn schema_rechecks_go_one_at_a_time(cx: &mut TestAppContext) {
        use crate::test_server::followed_session;
        use serde_json::json;

        const SCHEMA: &str = "node Person {\n  slug: String @key\n}\n";
        let (session, served) = followed_session(SCHEMA, |_| json!([]), cx);
        let reads = |cx: &mut TestAppContext| {
            cx.run_until_parked();
            std::mem::take(&mut served.lock().unwrap().schema_reads)
        };
        reads(cx);

        session.update(cx, |session, cx| {
            for _ in 0..3 {
                session.recheck_schema(None, cx);
            }
        });
        assert_eq!(reads(cx), 1);

        session.update(cx, |session, cx| {
            session.recheck_schema(None, cx);
            // Its read has left: reasons met now ask once more after it.
            session
                .schema_recheck
                .as_ref()
                .unwrap()
                .store(true, Ordering::SeqCst);
            session.recheck_schema(None, cx);
            session.recheck_schema(None, cx);
        });
        assert_eq!(reads(cx), 2);

        served.lock().unwrap().commit("c2", Vec::new());
        session.update(cx, |session, cx| {
            session.committed("main".into(), "c2".into(), Some("c1".into()), cx)
        });
        assert_eq!(reads(cx), 0);
        assert_eq!(
            session.read_with(cx, |session, _| session.head("main").map(str::to_owned)),
            Some("c2".into())
        );
    }

    /// A head move reads the change feed after the commit the log last holds, asked only about
    /// the types views read; a range answers once that read is in, and is unknown when nobody
    /// read the branch or the schema changed meanwhile.
    #[gpui::test]
    async fn a_head_move_reads_the_change_feed_for_the_types_views_read(cx: &mut TestAppContext) {
        use crate::carry::{Since, TypeRef};
        use crate::test_server::{followed_session, node_change};
        use serde_json::json;

        const SCHEMA: &str = "node Person {\n  slug: String @key\n}\nnode Company {\n  slug: String @key\n}\nedge WorksAt: Person -> Company\n";
        let (session, served) = followed_session(SCHEMA, |_| json!([]), cx);
        cx.run_until_parked();
        assert_eq!(
            session.read_with(cx, |session, _| session.head("main").map(str::to_owned)),
            Some("c1".into())
        );
        let commit = |cx: &mut TestAppContext, id: &str, changes| {
            served.lock().unwrap().commit(id, changes);
            session.update(cx, |session, cx| {
                session.saw_head("main".into(), id.into(), false, None, cx)
            });
        };
        let since = |cx: &mut TestAppContext, from: &str, to: &str| {
            session.read_with(cx, |session, _| session.changes_since("main", from, to))
        };
        let feeds = || served.lock().unwrap().feeds.clone();
        let person = TypeRef::Node("Person".into());
        let company = TypeRef::Node("Company".into());

        let watch = session.update(cx, |session, _| watching(session, &["Person"], &[]));
        commit(cx, "c2", vec![node_change("Company", "co1")]);
        assert!(
            matches!(since(cx, "c1", "c2"), Since::Pending),
            "the read is under way"
        );
        cx.run_until_parked();
        assert_eq!(feeds().len(), 1);
        assert!(
            feeds()[0].contains("start=after%3Ac1") && feeds()[0].contains("type=Person"),
            "{}",
            feeds()[0]
        );
        let Since::Known(range) = since(cx, "c1", "c2") else {
            panic!("known")
        };
        assert!(
            !range.type_changed(&person),
            "Company's change is no Person's"
        );
        assert!(
            range.type_changed(&company),
            "the feed was not asked about Company"
        );

        commit(cx, "c3", vec![node_change("Person", "p1")]);
        cx.run_until_parked();
        assert!(feeds()[1].contains("start=after%3Ac2"));
        let Since::Known(range) = since(cx, "c1", "c3") else {
            panic!("known")
        };
        assert!(range.entity_changed(&person, "p1"));
        assert!(!range.entity_changed(&person, "p2"));

        // Nobody reads the branch: no feed read, and the range is not known.
        drop(watch);
        commit(cx, "c4", vec![node_change("Person", "p2")]);
        cx.run_until_parked();
        assert_eq!(feeds().len(), 2);
        assert!(matches!(since(cx, "c3", "c4"), Since::Unknown));

        // A schema changed in between: what the feed said does not count.
        let _watch = session.update(cx, |session, _| watching(session, &["Person"], &[]));
        served.lock().unwrap().schema =
            format!("{SCHEMA}node Project {{\n  slug: String @key\n}}\n");
        commit(cx, "c5", vec![node_change("Company", "co2")]);
        cx.run_until_parked();
        assert_eq!(feeds().len(), 3);
        assert!(matches!(since(cx, "c4", "c5"), Since::Unknown));
    }

    /// A read at a commit that misses is answered by the same read at an earlier commit of the
    /// branch when no commit in between changed what it depends on: for a node's neighbours,
    /// the edges at the node and the neighbours listed (spec 015).
    #[gpui::test]
    async fn a_read_at_a_new_head_is_answered_from_an_earlier_commit_it_still_holds_for(
        cx: &mut TestAppContext,
    ) {
        use crate::test_server::{followed_session, node_change};
        use serde_json::json;

        const SCHEMA: &str =
            "node Person {\n  slug: String @key\n  name: String\n}\nedge Knows: Person -> Person\n";
        let sent = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen = sent.clone();
        let (session, served) = followed_session(
            SCHEMA,
            move |request| {
                seen.lock()
                    .unwrap()
                    .push(request["name"].as_str().unwrap_or_default().to_owned());
                json!([{ "id": "p2", "edge_id": "e1", "label": "Grace" }])
            },
            cx,
        );
        cx.run_until_parked();
        let schema = session.read_with(cx, |session, _| session.schema_model().unwrap());
        let _watch = session.update(cx, |session, _| watching(session, &["Person"], &["Knows"]));
        let group = schema.relationships_of("Person")[0].clone();
        let neighbours = builder::neighbours(
            &schema,
            "Person",
            &group,
            "p1",
            None,
            builder::Narrow::default(),
            51,
        )
        .unwrap();
        let sent_now = |name: &str| sent.lock().unwrap().iter().filter(|n| *n == name).count();
        let read_at = |cx: &mut TestAppContext, commit: &str| {
            let read = session.update(cx, |session, cx| {
                session.read(&neighbours, Some(commit.into()), cx)
            });
            cx.run_until_parked();
            read
        };
        let commit = |cx: &mut TestAppContext, id: &str, changes| {
            served.lock().unwrap().commit(id, changes);
            session.update(cx, |session, cx| session.check_heads(cx));
            cx.run_until_parked();
        };
        let edge = |id: &str, from: &str, to: &str| json!({ "kind": "edge", "type": { "id": "Knows", "name": "Knows" }, "id": id, "op": "insert", "after": { "properties": {}, "endpoints": { "from": from, "to": to } } });

        read_at(cx, "c1").await.unwrap();
        assert_eq!(sent_now("studio_neighbours"), 1);

        // Another person, and an edge between two others.
        commit(
            cx,
            "c2",
            vec![node_change("Person", "p9"), edge("e9", "p7", "p8")],
        );
        let carried = read_at(cx, "c2").await.unwrap();
        assert_eq!(sent_now("studio_neighbours"), 1, "answered from c1");
        assert_eq!(carried.graph_commit_id.as_deref(), Some("c2"));
        assert_eq!(carried.target.snapshot.as_deref(), Some("c2"));

        // A neighbour it lists changed.
        commit(cx, "c3", vec![node_change("Person", "p2")]);
        read_at(cx, "c3").await.unwrap();
        assert_eq!(sent_now("studio_neighbours"), 2, "read again");

        // An edge at the node.
        commit(cx, "c4", vec![edge("e10", "p3", "p1")]);
        read_at(cx, "c4").await.unwrap();
        assert_eq!(sent_now("studio_neighbours"), 3, "read again");
    }

    /// An answer kept at a commit is kept under the schema it was read with: a read at an old
    /// commit uses the current schema, so after the schema changes the same query is read
    /// again. A head move whose feed read finds another schema takes it (spec 015).
    /// A page's rows answer the read of each node at that commit, with no request of their own,
    /// filed by the read the views asking for the page share; a page answered from what was
    /// kept is not filed again.
    #[gpui::test]
    async fn a_page_read_at_a_commit_answers_its_nodes(cx: &mut TestAppContext) {
        use crate::test_server::followed_session;
        use serde_json::json;

        const SCHEMA: &str = "node Person {\n  slug: String @key\n  name: String\n}\n";
        let sent = Arc::new(std::sync::Mutex::new(0usize));
        let counted = sent.clone();
        let (session, _) = followed_session(
            SCHEMA,
            move |request| {
                if request["name"] == "studio_node" {
                    *counted.lock().unwrap() += 1;
                }
                json!([{ "n": { "@id": "p1", "slug": "p1", "name": "Ada" } }])
            },
            cx,
        );
        cx.run_until_parked();
        let schema = session.read_with(cx, |session, _| session.schema_model().unwrap());
        let page = builder::node_page(&schema, "Person", &[], None, &builder::PageStart::First, 10)
            .unwrap();
        let node = builder::node_by_id(&schema, "Person", "p1").unwrap();
        let read = |query: &BuiltQuery, cx: &mut TestAppContext| {
            let read = session.update(cx, |session, cx| session.read(query, Some("c1".into()), cx));
            cx.run_until_parked();
            read
        };
        let reads = || *sent.lock().unwrap();

        // Two views share the page's read; the one that asked first stops waiting. The read
        // files its rows itself, once, for whoever still waits.
        let (first, second) = session.update(cx, |session, cx| {
            (
                session.read(&page, Some("c1".into()), cx),
                session.read(&page, Some("c1".into()), cx),
            )
        });
        drop(first);
        cx.run_until_parked();
        second.await.unwrap();
        read(&node, cx).await.unwrap();
        assert_eq!(reads(), 0, "the page's row answered it");

        // The node's answer goes; the page, read again from what was kept, files nothing.
        session.update(cx, |session, _| {
            let key = session.pinned_key(&node, "c1");
            session.seeded_reads.forget(&key);
        });
        read(&page, cx).await.unwrap();
        read(&node, cx).await.unwrap();
        assert_eq!(reads(), 1);
    }

    #[gpui::test]
    async fn answers_kept_under_one_schema_are_not_given_under_another(cx: &mut TestAppContext) {
        use crate::carry::Since;
        use crate::test_server::{followed_session, node_change};
        use serde_json::json;

        const SCHEMA: &str = "node Person {\n  slug: String @key\n  name: String\n}\n";
        let sent = Arc::new(std::sync::Mutex::new(0usize));
        let counted = sent.clone();
        let (session, served) = followed_session(
            SCHEMA,
            move |request| {
                if request["name"] == "studio_node" {
                    *counted.lock().unwrap() += 1;
                }
                json!([{ "n": { "@id": "p1", "slug": "p1", "name": "Ada" } }])
            },
            cx,
        );
        cx.run_until_parked();
        let schema = session.read_with(cx, |session, _| session.schema_model().unwrap());
        let node = builder::node_by_id(&schema, "Person", "p1").unwrap();
        let read_at_c1 = |cx: &mut TestAppContext| {
            let read = session.update(cx, |session, cx| session.read(&node, Some("c1".into()), cx));
            cx.run_until_parked();
            read
        };
        let reads = || *sent.lock().unwrap();

        read_at_c1(cx).await.unwrap();
        read_at_c1(cx).await.unwrap();
        assert_eq!(reads(), 1, "kept at the commit");

        // The schema is read again with another text: the same read at the same commit is sent.
        served.lock().unwrap().schema = format!("{SCHEMA}node Note {{\n  slug: String @key\n}}\n");
        session.update(cx, |session, cx| session.reload_schema(cx));
        cx.run_until_parked();
        read_at_c1(cx).await.unwrap();
        assert_eq!(reads(), 2, "read again under the new schema");

        // A schema read again that cannot be read leaves the one held, and is not news; while
        // it is read, the one held is still there.
        served.lock().unwrap().schema_fails = true;
        let held_before = session.read_with(cx, |session, _| session.schema_model());
        session.update(cx, |session, cx| session.reload_schema(cx));
        assert!(
            session
                .read_with(cx, |session, _| session.schema_model())
                .is_some()
        );
        cx.run_until_parked();
        let held_after = session.read_with(cx, |session, _| session.schema_model());
        assert!(
            held_after.is_some_and(|after| held_before.is_some_and(|before| Arc::ptr_eq(&after, &before))),
            "the schema held stays"
        );
        served.lock().unwrap().schema_fails = false;

        // Another writer changes the schema; the head move's feed read finds it and the session
        // takes it, telling views it moved rather than changing them.
        let _watch = session.update(cx, |session, _| watching(session, &["Person"], &["Knows"]));
        let events = Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));
        let heard = events.clone();
        cx.update(|cx| {
            cx.subscribe(&session, move |_, event: &SessionEvent, _| match event {
                SessionEvent::SchemaChanged => heard.lock().unwrap().push("changed"),
                SessionEvent::SchemaMoved => heard.lock().unwrap().push("moved"),
                _ => {}
            })
            .detach()
        });
        let renamed = "node Person {\n  slug: String @key\n  full_name: String @rename_from(\"name\")\n}\nnode Note {\n  slug: String @key\n}\n";
        served.lock().unwrap().schema = renamed.into();
        served
            .lock()
            .unwrap()
            .commit("c2", vec![node_change("Note", "n1")]);
        session.update(cx, |session, cx| session.check_heads(cx));
        cx.run_until_parked();
        let held = session.read_with(cx, |session, _| {
            session.schema_model().unwrap().source.clone()
        });
        assert_eq!(&*held, renamed, "the new schema is taken");
        assert_eq!(*events.lock().unwrap(), ["moved"], "views are not changed");
        assert!(
            matches!(
                session.read_with(cx, |session, _| session.changes_since("main", "c1", "c2")),
                Since::Unknown
            ),
            "nothing carries across a schema change"
        );
        read_at_c1(cx).await.unwrap();
        assert_eq!(reads(), 3, "read again under the renamed schema");

        // A graph's node names (spec 002, "Node names"): a choice in the settings names the
        // type's nodes by it at once, announced as a change of the same text; none goes back.
        events.lock().unwrap().clear();
        let key = session.read_with(cx, |session, cx| session.overrides_key(cx));
        let named = |cx: &mut TestAppContext| {
            session.read_with(cx, |session, _| {
                let schema = session.schema_model().unwrap();
                let name = schema.node("Person").unwrap().label_property();
                name.map(str::to_owned)
            })
        };
        let choose = |choice: Option<&str>, cx: &mut TestAppContext| {
            let choice = choice.map(str::to_owned);
            cx.update(|cx| {
                let mut settings = cx
                    .try_global::<studio_settings::StudioSettings>()
                    .cloned()
                    .unwrap_or_default();
                settings.set_label(&key, "Person", choice);
                cx.set_global(settings);
            });
            cx.run_until_parked();
        };
        assert_eq!(
            named(cx).as_deref(),
            Some("slug"),
            "no name or title: the key"
        );
        choose(Some("full_name"), cx);
        assert_eq!(named(cx).as_deref(), Some("full_name"));
        assert_eq!(
            *events.lock().unwrap(),
            ["changed"],
            "views take the names at once"
        );
        // A schema read again keeps the names chosen.
        session.update(cx, |session, cx| session.reload_schema(cx));
        cx.run_until_parked();
        assert_eq!(named(cx).as_deref(), Some("full_name"));
        choose(None, cx);
        assert_eq!(named(cx).as_deref(), Some("slug"));
    }

    /// What a read's rows also answer is kept: a page's whole nodes answer their node reads,
    /// and a page's counts each node's count, at the commit read. A count carries to a later
    /// commit that changed no edge of its group at the node (spec 015).
    #[gpui::test]
    async fn a_pages_rows_answer_node_reads_and_its_counts_each_nodes_count(
        cx: &mut TestAppContext,
    ) {
        use crate::test_server::{followed_session, node_change};
        use serde_json::json;

        const SCHEMA: &str =
            "node Person {\n  slug: String @key\n  name: String\n}\nedge Knows: Person -> Person\n";
        let sent = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen = sent.clone();
        let (session, served) = followed_session(
            SCHEMA,
            move |request| {
                let name = request["name"].as_str().unwrap_or_default().to_owned();
                seen.lock().unwrap().push(name.clone());
                match name.as_str() {
                    "studio_page" => json!([
                        { "n": { "@id": "p1", "slug": "p1", "name": "Ada" } },
                        { "n": { "@id": "p2", "slug": "p2", "name": "Grace" } },
                    ]),
                    "studio_page_degrees" => json!([{ "id": "p1", "n": 3 }]),
                    _ => json!([]),
                }
            },
            cx,
        );
        cx.run_until_parked();
        let schema = session.read_with(cx, |session, _| session.schema_model().unwrap());
        let sent_now = |name: &str| sent.lock().unwrap().iter().filter(|n| *n == name).count();
        let page = builder::node_page(&schema, "Person", &[], None, &builder::PageStart::First, 50)
            .unwrap();
        let read = session.update(cx, |session, cx| session.read(&page, None, cx));
        cx.run_until_parked();
        read.await.unwrap();

        let node = builder::node_by_id(&schema, "Person", "p2").unwrap();
        let read = session.update(cx, |session, cx| session.read(&node, Some("c1".into()), cx));
        let output = read.await.unwrap();
        assert_eq!(sent_now("studio_node"), 0, "the page answered it");
        assert_eq!(output.rows[0]["n"]["name"], json!("Grace"));

        let group = schema.relationships_of("Person")[0].clone();
        let ids = vec!["p1".to_owned(), "p2".to_owned()];
        let degrees = builder::page_degrees(&schema, "Person", &group, &ids).unwrap();
        let read = session.update(cx, |session, cx| {
            session.read(&degrees, Some("c1".into()), cx)
        });
        cx.run_until_parked();
        read.await.unwrap();
        let count = |cx: &mut TestAppContext, id: &str, commit: &str| {
            let read = session.update(cx, |session, cx| {
                session.degree_at("Person", &group, id, Some(commit.into()), cx)
            });
            cx.run_until_parked();
            async move { read.await.map(|(count, _)| count) }
        };
        assert_eq!(count(cx, "p1", "c1").await, Ok(3));
        assert_eq!(count(cx, "p2", "c1").await, Ok(0), "no row is none");
        assert_eq!(
            sent_now("studio_page_degrees"),
            1,
            "the counts were the page's"
        );

        // Another person changed, no edge at p1: its count carries.
        let _scope = session.update(cx, |session, _| watching(session, &["Person"], &["Knows"]));
        served
            .lock()
            .unwrap()
            .commit("c2", vec![node_change("Person", "p9")]);
        session.update(cx, |session, cx| session.check_heads(cx));
        cx.run_until_parked();
        assert_eq!(count(cx, "p1", "c2").await, Ok(3));
        assert_eq!(sent_now("studio_page_degrees"), 1, "carried, not read");
    }
}
