//! One node's history (spec 046): the commits of a branch's recent first-parent history that
//! changed the node or one of its relationships, newest first, each with who made it and how;
//! who last set each property (blame); and runs of one actor's small edits folded into one.
//!
//! Omnigraph has no read of the commits that changed one entity (E62). Studio reads a window of
//! the branch's newest [`NODE_HISTORY_COMMITS`] first-parent commits instead: the commit listing
//! names the window, and the branch's change feed from just before its oldest commit, filtered to
//! the node's type and the relationship types that end at it, answers every commit of the window
//! in a few pages (each covers up to 128 commits, E38). The feed is read twice, as the design
//! study measured (`docs/ux/2026-11-20-node-history.md`): updates and deletes first, which cost
//! what was edited, then inserts under [`NODE_HISTORY_INSERT_CAP`] a window, since a load of new
//! nodes of the type costs its whole images however little it did to this node. An older window
//! goes on from the listing's page token, for the head the first one resolved (constitution II).
//!
//! Pure functions and types; the session sends the reads
//! ([`crate::GraphSession::node_history_in`]).

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use gpui::{AsyncApp, Context, Task, WeakEntity};
use gq_language::SchemaModel;
use graph_client::api::{
    ChangeBlockOutput, ChangeCauseOutput, ChangeDiffRefusalReason, ChangeFeedOutput,
    ChangeImageOutput, EntityChangeOutput, EntityKind,
};
use graph_client::{ApiErrorSummary, FeedAt, FeedFilter, FeedOp, GraphClient, ReadError};
use serde_json::Value;

use crate::GraphSession;
use crate::changes::{FoldedChange, NetChange, endpoints_of};
use crate::ledger::Asker;
use crate::scope::ReadScope;

/// First-parent commits a window of a node's history covers: two feed pages on a large graph,
/// read in about a second on the development graph (study, "Cost"), with "Read the 200 commits
/// before" as the honest exit.
pub const NODE_HISTORY_COMMITS: u32 = 200;

/// Changes a feed page holds at most: the server's largest page, so a window is as few requests
/// as the server allows (E36).
pub const NODE_HISTORY_FEED_LIMIT: u32 = 8_192;

/// Feed pages one pass over a window reads at most. A page ends on a whole commit and covers up
/// to 128 commits, so a window needs two; more only where the node's types changed so often that
/// a page's change limit ended it first.
pub const NODE_HISTORY_FEED_PAGES: usize = 12;

/// Bytes the insert pass of one window reads at most (8 MiB). A load of 50,000 new nodes of the
/// type cost 13.2 MiB in the study for a window that changed the node six times; updates and
/// deletes of the same window cost 0.3 MiB.
pub const NODE_HISTORY_INSERT_CAP: usize = 8 * 1024 * 1024;

/// Merges in a window whose branch Studio names, one commit read each: past them an entry says
/// "via merge of commit …".
pub const NODE_HISTORY_MERGE_NAMES: usize = 20;

/// How far apart two edits of one run may be (10 minutes, Notion's version window), in Unix
/// microseconds.
pub const FOLD_WINDOW: i64 = 10 * 60 * 1_000_000;

/// What a node's history reads: the node's type and id, and the relationship types that can end
/// at it, with the ends that are its type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeScope {
    pub type_name: String,
    pub id: String,
    /// Each relationship type that ends at the type: its name, and whether its source and its
    /// target are of the node's type.
    edges: Vec<(String, bool, bool)>,
}

impl NodeScope {
    /// The node `id` of `type_name`, with every relationship type the schema ends at its type.
    pub fn new(schema: &SchemaModel, type_name: &str, id: &str) -> Self {
        let edges = schema
            .edges
            .iter()
            .filter(|edge| edge.from == type_name || edge.to == type_name)
            .map(|edge| {
                (
                    edge.name.clone(),
                    edge.from == type_name,
                    edge.to == type_name,
                )
            })
            .collect();
        Self {
            type_name: type_name.to_owned(),
            id: id.to_owned(),
            edges,
        }
    }

    /// The types the feed is filtered by: the node's, then its relationships'.
    pub fn types(&self) -> Vec<String> {
        std::iter::once(self.type_name.clone())
            .chain(self.edges.iter().map(|(name, _, _)| name.clone()))
            .collect()
    }

    /// Whether `change` is to the node or to one of its relationships, at either end.
    pub fn holds(&self, change: &EntityChangeOutput) -> bool {
        match change.kind {
            EntityKind::Node => change.entity_type.name == self.type_name && change.id == self.id,
            EntityKind::Edge => {
                let Some((_, from, to)) = self
                    .edges
                    .iter()
                    .find(|(name, _, _)| *name == change.entity_type.name)
                else {
                    return false;
                };
                [change.before.as_ref(), change.after.as_ref()]
                    .into_iter()
                    .flatten()
                    .filter_map(|image| endpoints_of(image.endpoints.as_ref()?))
                    .any(|(source, target)| {
                        (*from && source == self.id) || (*to && target == self.id)
                    })
            }
        }
    }

    /// `blocks` with only the changes to the node and its relationships: every block stays, empty
    /// or not, since a block of a commit past the window is what ends it. Run where the page is
    /// decoded, off the main thread, so a load's images never reach it.
    pub fn keep(&self, blocks: Vec<ChangeBlockOutput>) -> Vec<ChangeBlockOutput> {
        blocks
            .into_iter()
            .map(|mut block| {
                block.changes.retain(|change| self.holds(change));
                block
            })
            .collect()
    }
}

/// One commit's changes to the node and its relationships.
#[derive(Debug, Clone)]
pub struct NodeHistoryEntry {
    pub commit: String,
    pub actor: Option<String>,
    /// When the commit was authored, Unix microseconds.
    pub at: i64,
    /// The branch the commit was authored on, as the feed says.
    pub branch: String,
    /// The merged branch's head, for a merge: the change is the branch's, made there by others
    /// (E62).
    pub merged_parent: Option<String>,
    /// The branch the merged head was made on, once read: "via merge of …".
    pub merged_branch: Option<String>,
    /// What the commit did to the node itself.
    pub node: Option<FoldedChange>,
    /// What it did to the node's relationships, in the feed's order.
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

    /// The commit was authored on an agent's branch (`agent/<stamp>`, spec 013): the agent's own
    /// credential is not recorded yet (E20), so the branch is what says so.
    pub fn by_agent(&self) -> bool {
        self.branch.starts_with("agent/")
    }
}

/// Where an older window of a node's history goes on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OlderWindow {
    /// The commit listing's continuation, for the head the first window resolved: its page
    /// names the older window's commits.
    pub page_token: String,
}

/// What a window of a node's history may read: the first, from the branch head, or an older
/// one, below the windows read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeHistoryWindow {
    Newest,
    Older { head: String, window: OlderWindow },
}

/// A commit of the window, as the listing named it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowCommit {
    pub id: String,
    pub parent: Option<String>,
    /// Unix microseconds.
    pub at: i64,
}

/// The insert pass stopped at its cap: the new nodes and relationships of the window's newest
/// `commits` commits were not read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InsertsSkipped {
    pub commits: usize,
    /// The newest commit whose inserts were not read, and when it was made: a value the window
    /// does not show being set was set at or before it.
    pub newest: WindowCommit,
}

/// A schema change the feed would not read across (409 `schema_boundary`, E52): the window's
/// commits from it back were not read, and no older window is offered.
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
    /// The commits that changed the node or its relationships, newest first.
    pub entries: Vec<NodeHistoryEntry>,
    /// The window's commits, newest first, as listed; those past a schema boundary left out.
    pub commits: Vec<WindowCommit>,
    /// Where an older window goes on; `None` once the window reached the graph's first commit,
    /// a schema boundary or history cleaned up.
    pub older: Option<OlderWindow>,
    /// Why the window was not read to its end, in words, when it was not: what it holds is what
    /// was read before that.
    pub stopped: Option<String>,
    pub inserts_skipped: Option<InsertsSkipped>,
    pub boundary: Option<SchemaBoundary>,
    /// The history before the window was cleaned up (410).
    pub cleaned_up: bool,
    /// Requests sent and bytes of change feed read, for the end row and the validation log.
    pub requests: usize,
    pub feed_bytes: usize,
}

/// One pass of the feed over a window: the blocks of the window's commits, until the first
/// block of a commit the listing did not name, which is past the window (the head the first
/// window resolved, or the window above an older one). The bound does not rest on the feed
/// sending a block for every commit, which a feed filtered by type need not do (review of #146).
#[derive(Debug)]
pub struct PassFold {
    window: HashSet<String>,
    blocks: HashMap<String, (ChangeCauseOutput, Vec<EntityChangeOutput>)>,
    /// The window's commits read, and the newest of them, by its place in the listing.
    read: usize,
    done: bool,
    newest_read: Option<String>,
}

impl PassFold {
    /// A pass over `commits`, the ids a listing page named.
    pub fn new<'a>(commits: impl IntoIterator<Item = &'a WindowCommit>) -> Self {
        Self {
            window: commits.into_iter().map(|c| c.id.clone()).collect(),
            blocks: HashMap::new(),
            read: 0,
            done: false,
            newest_read: None,
        }
    }

    /// Takes a page's blocks, oldest first; says whether the window is read to its end.
    pub fn page(&mut self, blocks: Vec<ChangeBlockOutput>) -> bool {
        for block in blocks {
            if self.done {
                break;
            }
            let ChangeBlockOutput { cause, changes } = block;
            if !self.window.contains(&cause.graph_commit_id) {
                self.done = true;
                break;
            }
            self.read += 1;
            self.newest_read = Some(cause.graph_commit_id.clone());
            self.blocks
                .insert(cause.graph_commit_id.clone(), (cause, changes));
            if self.read == self.window.len() {
                self.done = true;
            }
        }
        self.done
    }

    pub fn done(&self) -> bool {
        self.done
    }

    /// The feed was read to the branch's head: the window's commits it sent no block for changed
    /// none of the node's types.
    pub fn caught_up(&mut self) {
        self.done = true;
    }

    /// Whether no block was read yet.
    pub fn is_empty(&self) -> bool {
        self.read == 0
    }

    /// The newest commit a block was read for.
    pub fn newest_read(&self) -> Option<&str> {
        self.newest_read.as_deref()
    }
}

/// The window's entries, newest first: each commit of `commits` (newest first, as listed) that
/// either pass found changing the node or one of its relationships, its changes from both
/// passes together. A change that ends where it began is left out.
pub fn entries_of(commits: &[WindowCommit], passes: Vec<PassFold>) -> Vec<NodeHistoryEntry> {
    let mut passes: Vec<_> = passes.into_iter().map(|pass| pass.blocks).collect();
    let mut entries = Vec::new();
    for commit in commits {
        let mut cause = None;
        let mut changes = Vec::new();
        for blocks in &mut passes {
            if let Some((c, mut more)) = blocks.remove(&commit.id) {
                cause.get_or_insert(c);
                changes.append(&mut more);
            }
        }
        let Some(cause) = cause else {
            continue;
        };
        let mut entry = NodeHistoryEntry {
            commit: cause.graph_commit_id.clone(),
            actor: cause.actor_id.clone(),
            at: cause.authored_at,
            branch: cause.authored_branch.clone(),
            merged_parent: cause.merged_parent_commit_id.clone(),
            merged_branch: None,
            node: None,
            edges: Vec::new(),
        };
        for change in changes {
            let is_edge = matches!(change.kind, EntityKind::Edge);
            let folded = FoldedChange {
                kind: change.kind,
                type_id: change.entity_type.id,
                type_name: change.entity_type.name,
                id: change.id,
                before: change.before,
                after: change.after,
                commits: vec![cause.graph_commit_id.clone()],
            };
            if folded.net().is_none() {
                continue;
            }
            if is_edge {
                entry.edges.push(folded);
            } else {
                entry.node = Some(folded);
            }
        }
        if entry.node.is_some() || !entry.edges.is_empty() {
            entries.push(entry);
        }
    }
    entries
}

/// The schema boundary a refused feed read names, when that is why it was refused.
pub fn boundary_of(error: &ReadError) -> Option<SchemaBoundary> {
    let ReadError::Refused(ApiErrorSummary {
        status: 409,
        change_diff_refusal: Some(refusal),
        ..
    }) = error
    else {
        return None;
    };
    (refusal.reason == ChangeDiffRefusalReason::SchemaBoundary).then(|| SchemaBoundary {
        commit: refusal.graph_commit_id.clone(),
        type_name: refusal.type_name.clone(),
    })
}

/// Who last set one property, within what was read ("Now", spec 046).
#[derive(Debug, Clone, PartialEq)]
pub struct Blame {
    pub property: String,
    /// The value now, from the newest image read of the node; `None` when no commit read changed
    /// the node itself, so no image of it was read.
    pub value: Option<Value>,
    pub set: BlameSet,
}

/// How a property's value came to be, as far as the history read reaches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlameSet {
    /// Entry `entry` (an index into the entries, newest first) set it; `created` when it was
    /// the node's creation.
    In { entry: usize, created: bool },
    /// No commit read set it: it was set at or before this commit, the oldest read or the newest
    /// whose inserts were not read.
    Before(WindowCommit),
}

/// The node's properties (`properties`, the schema's order), each with the newest entry of
/// `entries` (newest first) that set it: an update whose images differ there, or the node's
/// creation, which sets every property. Past them, "before" `bound`.
pub fn blame(
    entries: &[NodeHistoryEntry],
    properties: &[String],
    bound: Option<&WindowCommit>,
) -> Vec<Blame> {
    let image = entries
        .iter()
        .find_map(|entry| entry.node.as_ref())
        .and_then(|node| node.after.as_ref().or(node.before.as_ref()));
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
            let set = match (set, bound) {
                (Some(set), _) => set,
                (None, Some(bound)) => BlameSet::Before(bound.clone()),
                // Nothing was read at all.
                (None, None) => return None,
            };
            Some(Blame {
                property: name.clone(),
                value: image.and_then(|image| image.properties.get(name).cloned()),
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

/// The newest image of the node the entries read, and whether it is the image of its removal.
pub fn newest_image(entries: &[NodeHistoryEntry]) -> Option<(&ChangeImageOutput, bool)> {
    let node = entries.iter().find_map(|entry| entry.node.as_ref())?;
    match &node.after {
        Some(after) => Some((after, false)),
        None => node.before.as_ref().map(|before| (before, true)),
    }
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

/// Who asks, through which client and of which graph and branch: what each read of a window
/// needs.
struct Reader {
    session: WeakEntity<GraphSession>,
    asker: Asker,
    client: GraphClient,
    graph: String,
    branch: String,
    node: NodeScope,
    requests: usize,
    feed_bytes: usize,
}

/// How one pass over a window ended.
enum PassEnd {
    /// Read to the window's end.
    Read,
    /// The insert pass's cap was reached.
    Capped,
    Boundary(SchemaBoundary),
    CleanedUp,
    /// Stopped short, in words: what was read stays.
    Stopped(String),
}

impl Reader {
    /// Sends `request` through the session, counted and named in its ledger as `what`.
    async fn send<T: Send + 'static>(
        &mut self,
        what: &'static str,
        request: impl Future<Output = Result<T, ReadError>> + Send + 'static,
        cx: &mut AsyncApp,
    ) -> Result<T, ReadError> {
        self.requests += 1;
        let asker = self.asker;
        let read = self
            .session
            .update(cx, |session, cx| {
                session.counted_read_by(what, asker, request, cx)
            })
            .map_err(|_| ReadError::Transport("Studio closed the graph".into()))?;
        read.await
    }

    /// One pass of the feed from `start` over `fold`'s window, keeping `ops`, under `cap` bytes
    /// when one is given.
    async fn pass(
        &mut self,
        what: &'static str,
        start: String,
        ops: &'static [FeedOp],
        cap: Option<usize>,
        fold: &mut PassFold,
        cx: &mut AsyncApp,
    ) -> Result<PassEnd, ReadError> {
        let types = self.node.types();
        let mut next = Some(FeedNext::Start(start));
        let mut spent = 0usize;
        for _ in 0..NODE_HISTORY_FEED_PAGES {
            let Some(at) = next.take() else {
                break;
            };
            let budget = match cap {
                Some(cap) if spent >= cap => return Ok(PassEnd::Capped),
                Some(cap) => cap - spent,
                None => usize::MAX,
            };
            let (client, graph, branch) =
                (self.client.clone(), self.graph.clone(), self.branch.clone());
            let (node, types) = (self.node.clone(), types.clone());
            // Filtered where it is decoded, off the main thread: a load's images never reach it.
            let request = async move {
                let filter = FeedFilter { types: &types, ops };
                let (page, bytes) = client
                    .changes_within(
                        &graph,
                        &branch,
                        at.at(),
                        &filter,
                        NODE_HISTORY_FEED_LIMIT,
                        budget,
                    )
                    .await?;
                let ChangeFeedOutput {
                    blocks,
                    next_page_token,
                    cursor,
                    caught_up,
                } = page;
                let blocks = node.keep(blocks);
                Ok((blocks, next_page_token, cursor, caught_up, bytes))
            };
            match self.send(what, request, cx).await {
                Ok((blocks, next_page_token, cursor, caught_up, bytes)) => {
                    spent += bytes;
                    self.feed_bytes += bytes;
                    if fold.page(blocks) {
                        return Ok(PassEnd::Read);
                    }
                    next = match (next_page_token, cursor, caught_up) {
                        (Some(token), ..) => Some(FeedNext::Page(token)),
                        (None, _, Some(true)) | (None, None, _) => None,
                        (None, Some(cursor), _) => Some(FeedNext::Cursor(cursor)),
                    };
                }
                Err(ReadError::TooLarge { .. }) if cap.is_some() => return Ok(PassEnd::Capped),
                Err(error) => {
                    if let Some(boundary) = boundary_of(&error) {
                        return Ok(PassEnd::Boundary(boundary));
                    }
                    if matches!(&error, ReadError::Refused(refusal) if refusal.status == 410) {
                        return Ok(PassEnd::CleanedUp);
                    }
                    if fold.is_empty() {
                        return Err(error);
                    }
                    return Ok(PassEnd::Stopped(error.to_string()));
                }
            }
        }
        if next.is_none() {
            fold.caught_up();
            return Ok(PassEnd::Read);
        }
        Ok(PassEnd::Stopped(format!(
            "Studio stopped after {NODE_HISTORY_FEED_PAGES} pages of changes"
        )))
    }
}

/// Where the next page of a pass starts.
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

/// Where the feed starts for a window whose oldest commit is `oldest`: just before it.
fn start_before(oldest: &WindowCommit) -> String {
    match &oldest.parent {
        Some(parent) => format!("after:{parent}"),
        None => "beginning".to_owned(),
    }
}

/// The operations of the first pass, and of the second.
const EDITS: &[FeedOp] = &[FeedOp::Update, FeedOp::Delete];
const INSERTS: &[FeedOp] = &[FeedOp::Insert];

impl GraphSession {
    /// A window of node `node`'s history on `branch` (spec 046), asked for through `scope`: the
    /// newest [`NODE_HISTORY_COMMITS`] first-parent commits, or the ones before a window read
    /// already. One commit listing, then the change feed from just before its oldest commit,
    /// filtered to the node's types: its updates and deletes, then its inserts under
    /// [`NODE_HISTORY_INSERT_CAP`], then the branch of each merge that changed the node, each read
    /// named in the ledger. Nothing is asked again by itself. The newest window keeps the node's
    /// last change for its page ([`Self::last_change`]).
    pub fn node_history_in(
        &mut self,
        scope: &ReadScope,
        branch: &str,
        node: NodeScope,
        window: NodeHistoryWindow,
        cx: &mut Context<Self>,
    ) -> Task<Result<NodeHistoryPage, ReadError>> {
        let mut reader = Reader {
            session: cx.entity().downgrade(),
            asker: self.asker_of(scope),
            client: self.client(cx),
            graph: self.graph_id.clone(),
            branch: branch.to_owned(),
            node,
            requests: 0,
            feed_bytes: 0,
        };
        cx.spawn(async move |_, cx| {
            let newest = matches!(window, NodeHistoryWindow::Newest);
            let (token, head) = match window {
                NodeHistoryWindow::Newest => (None, None),
                NodeHistoryWindow::Older { head, window } => (Some(window.page_token), Some(head)),
            };
            let (client, graph, branch) = (
                reader.client.clone(),
                reader.graph.clone(),
                reader.branch.clone(),
            );
            let listing = reader
                .send(
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
            let mut commits: Vec<WindowCommit> = listing
                .commits
                .iter()
                .map(|c| WindowCommit {
                    id: c.graph_commit_id.clone(),
                    parent: c.parent_commit_id.clone(),
                    at: c.created_at,
                })
                .collect();
            let mut older = listing
                .next_page_token
                .clone()
                .map(|page_token| OlderWindow { page_token });
            let mut page = NodeHistoryPage {
                head,
                entries: Vec::new(),
                commits: Vec::new(),
                older: None,
                stopped: None,
                inserts_skipped: None,
                boundary: None,
                cleaned_up: false,
                requests: 0,
                feed_bytes: 0,
            };
            let Some(oldest) = commits.last() else {
                page.requests = reader.requests;
                return Ok(page);
            };
            let mut start = start_before(oldest);

            // Updates and deletes. A schema change in the window ends the history there: the
            // feed is read again from just after it, for the commits since.
            let mut edits = PassFold::new(&commits);
            let mut end = reader
                .pass(
                    "node_history_changes",
                    start.clone(),
                    EDITS,
                    None,
                    &mut edits,
                    cx,
                )
                .await?;
            if let PassEnd::Boundary(boundary) = &end {
                let at = commits.iter().position(|c| c.id == boundary.commit);
                commits.truncate(at.unwrap_or(0));
                older = None;
                page.boundary = Some(boundary.clone());
                start = format!("after:{}", boundary.commit);
                edits = PassFold::new(&commits);
                end = if commits.is_empty() {
                    PassEnd::Read
                } else {
                    reader
                        .pass(
                            "node_history_changes",
                            start.clone(),
                            EDITS,
                            None,
                            &mut edits,
                            cx,
                        )
                        .await?
                };
            }
            match end {
                PassEnd::Read | PassEnd::Capped => {}
                PassEnd::Boundary(_) => {
                    page.stopped = Some("the feed met a second schema change".into());
                }
                PassEnd::CleanedUp => {
                    page.cleaned_up = true;
                    older = None;
                }
                PassEnd::Stopped(why) => page.stopped = Some(why),
            }

            // Inserts, under the cap: the node's creation and the relationships added.
            let mut inserts = PassFold::new(&commits);
            if !commits.is_empty() && page.stopped.is_none() && !page.cleaned_up {
                let end = reader
                    .pass(
                        "node_history_inserts",
                        start,
                        INSERTS,
                        Some(NODE_HISTORY_INSERT_CAP),
                        &mut inserts,
                        cx,
                    )
                    .await;
                match end {
                    Ok(PassEnd::Read) => {}
                    Ok(PassEnd::Capped) => {
                        let read_to = inserts
                            .newest_read()
                            .and_then(|id| commits.iter().position(|c| c.id == id))
                            .unwrap_or(commits.len());
                        if read_to > 0 {
                            page.inserts_skipped = Some(InsertsSkipped {
                                commits: read_to,
                                newest: commits[0].clone(),
                            });
                        }
                    }
                    Ok(PassEnd::Boundary(_)) => {
                        page.stopped = Some("the feed met a schema change".into());
                    }
                    Ok(PassEnd::CleanedUp) => page.cleaned_up = true,
                    Ok(PassEnd::Stopped(why)) => page.stopped = Some(why),
                    // What the first pass read stays.
                    Err(error) => page.stopped = Some(error.to_string()),
                }
            }
            page.entries = entries_of(&commits, vec![edits, inserts]);

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
                    .send(
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
            page.older = older;
            page.requests = reader.requests;
            page.feed_bytes = reader.feed_bytes;
            if newest && let Some(last) = page.entries.first() {
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

    fn cause(commit: &str, actor: &str, at: i64) -> serde_json::Value {
        json!({
            "graph_commit_id": commit,
            "authored_branch": "main",
            "authored_at": at,
            "actor_id": actor,
        })
    }

    fn block(commit: &str, actor: &str, at: i64, changes: serde_json::Value) -> ChangeBlockOutput {
        serde_json::from_value(json!({ "cause": cause(commit, actor, at), "changes": changes }))
            .unwrap()
    }

    fn person(
        id: &str,
        op: &str,
        before: Option<serde_json::Value>,
        after: Option<serde_json::Value>,
    ) -> serde_json::Value {
        let image = |p: Option<serde_json::Value>| p.map(|p| json!({ "properties": p }));
        json!({
            "kind": "node", "type": {"id": "t-person", "name": "Person"}, "id": id, "op": op,
            "before": image(before), "after": image(after),
        })
    }

    fn edge(name: &str, op: &str, from: &str, to: &str) -> serde_json::Value {
        let image = json!({"properties": {}, "endpoints": {"from": from, "to": to}});
        let (before, after) = match op {
            "delete" => (Some(image), None),
            _ => (None, Some(image)),
        };
        json!({
            "kind": "edge", "type": {"id": format!("t-{name}"), "name": name},
            "id": format!("{name}:{from}:{to}"), "op": op, "before": before, "after": after,
        })
    }

    fn scope() -> NodeScope {
        NodeScope::new(&SchemaModel::parse(SCHEMA).unwrap(), "Person", "ada")
    }

    fn commits(ids: &[&str]) -> Vec<WindowCommit> {
        ids.iter()
            .enumerate()
            .map(|(n, id)| WindowCommit {
                id: (*id).to_owned(),
                parent: None,
                at: n as i64,
            })
            .collect()
    }

    #[test]
    fn the_scope_reads_the_type_and_the_relationships_at_either_end() {
        assert_eq!(scope().types(), ["Person", "Knows", "WorksAt"]);
        let knows_to_ada: EntityChangeOutput =
            serde_json::from_value(edge("Knows", "insert", "grace", "ada")).unwrap();
        let knows_from_ada: EntityChangeOutput =
            serde_json::from_value(edge("Knows", "insert", "ada", "grace")).unwrap();
        let other: EntityChangeOutput =
            serde_json::from_value(edge("Knows", "insert", "grace", "alan")).unwrap();
        assert!(scope().holds(&knows_to_ada), "the target end");
        assert!(scope().holds(&knows_from_ada), "the source end");
        assert!(!scope().holds(&other));
    }

    /// The two passes of a window land in one entry per commit, newest first: an update from
    /// the first, and a relationship added in the same commit from the insert pass.
    #[test]
    fn both_passes_of_a_commit_are_one_entry() {
        let window = commits(&["c3", "c2", "c1"]);
        let mut updates = PassFold::new(&window);
        updates.page(scope().keep(vec![
            block(
                "c1",
                "act-a",
                1,
                json!([person(
                    "ada",
                    "update",
                    Some(json!({"title": "A"})),
                    Some(json!({"title": "B"}))
                )]),
            ),
            block(
                "c3",
                "act-b",
                3,
                json!([person(
                    "bob",
                    "update",
                    Some(json!({"title": "A"})),
                    Some(json!({"title": "B"}))
                )]),
            ),
        ]));
        updates.caught_up();
        let mut inserts = PassFold::new(&window);
        inserts.page(scope().keep(vec![block(
            "c1",
            "act-a",
            1,
            json!([edge("Knows", "insert", "grace", "ada")]),
        )]));
        let entries = entries_of(&window, vec![updates, inserts]);
        assert_eq!(entries.len(), 1, "c3 changed another person");
        assert_eq!(entries[0].commit, "c1");
        assert!(entries[0].node.is_some());
        assert_eq!(entries[0].edges.len(), 1);
    }

    /// A pass ends at the first block past its window, with no block for every commit.
    #[test]
    fn a_pass_ends_at_the_first_block_past_its_window() {
        let window = commits(&["c2", "c1"]);
        let mut pass = PassFold::new(&window);
        assert!(pass.page(vec![
            block("c1", "act-a", 1, json!([])),
            block("c9", "act-a", 9, json!([])),
        ]));
        assert_eq!(pass.newest_read(), Some("c1"));
    }

    /// "Now": each property's newest setter within what was read, the creation for those no
    /// update set, and "before" the bound for a property no commit read set.
    #[test]
    fn blame_names_the_newest_setter_and_says_before_past_the_window() {
        let window = commits(&["c3", "c2", "c1"]);
        let mut pass = PassFold::new(&window);
        pass.page(vec![
            block(
                "c1",
                "act-a",
                1,
                json!([person(
                    "ada",
                    "update",
                    Some(json!({"name": "Ada", "title": "A", "email": "a@x"})),
                    Some(json!({"name": "Ada", "title": "B", "email": "a@x"}))
                )]),
            ),
            block(
                "c3",
                "act-b",
                3,
                json!([person(
                    "ada",
                    "update",
                    Some(json!({"name": "Ada", "title": "B", "email": "a@x"})),
                    Some(json!({"name": "Ada", "title": "B", "email": "b@x"}))
                )]),
            ),
        ]);
        let entries = entries_of(&window, vec![pass]);
        let props: Vec<String> = ["name", "title", "email"].map(String::from).into();
        let oldest = window.last().unwrap();
        let now = super::blame(&entries, &props, Some(oldest));
        assert_eq!(
            now[0],
            Blame {
                property: "name".into(),
                value: Some(json!("Ada")),
                set: BlameSet::Before(oldest.clone()),
            }
        );
        assert_eq!(
            now[1].set,
            BlameSet::In {
                entry: 1,
                created: false
            }
        );
        assert_eq!(now[1].value, Some(json!("B")));
        assert_eq!(
            now[2].set,
            BlameSet::In {
                entry: 0,
                created: false
            }
        );

        // A creation sets what no later update did.
        let window = commits(&["c2", "c1"]);
        let mut pass = PassFold::new(&window);
        pass.page(vec![
            block(
                "c1",
                "act-a",
                1,
                json!([person("ada", "insert", None, Some(json!({"name": "Ada"})))]),
            ),
            block(
                "c2",
                "act-b",
                2,
                json!([person(
                    "ada",
                    "update",
                    Some(json!({"name": "Ada", "title": null})),
                    Some(json!({"name": "Ada", "title": "T"}))
                )]),
            ),
        ]);
        let entries = entries_of(&window, vec![pass]);
        assert!(entries[1].created());
        let now = super::blame(&entries, &props[..2], window.last());
        assert_eq!(
            now[0].set,
            BlameSet::In {
                entry: 1,
                created: true
            }
        );
        assert_eq!(
            now[1].set,
            BlameSet::In {
                entry: 0,
                created: false
            }
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
                type_id: "t-person".into(),
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
        let studio = vec![false; entries.len()];
        let runs = runs(&entries, &studio);
        assert_eq!(runs, [0..3, 3..4, 4..5, 5..6]);
        let (node, edges) = net_of(&entries[runs[0].clone()]);
        let node = node.unwrap();
        assert_eq!(node.before.unwrap().properties["title"], json!("A"));
        assert_eq!(node.after.unwrap().properties["title"], json!("D"));
        assert_eq!(node.commits, ["c4", "c5", "c6"]);
        assert!(edges.is_empty());

        // Studio's own write and another writer's do not fold together.
        let studio = [true, false, false, false, false, false];
        assert_eq!(runs_of(&entries, &studio)[0], 0..1);
    }

    fn runs_of(entries: &[NodeHistoryEntry], studio: &[bool]) -> Vec<Range<usize>> {
        runs(entries, studio)
    }

    /// A refused feed read names its schema boundary.
    #[test]
    fn a_schema_boundary_is_read_from_the_refusal() {
        let error = ReadError::Refused(ApiErrorSummary {
            status: 409,
            message: "schema boundary".into(),
            code: None,
            diagnostic: None,
            change_diff_refusal: Some(Box::new(
                serde_json::from_value(json!({
                    "reason": "schema_boundary", "graph_commit_id": "c7", "type_name": "Person"
                }))
                .unwrap(),
            )),
            challenge: None,
            unauthorized: None,
        });
        assert_eq!(
            boundary_of(&error),
            Some(SchemaBoundary {
                commit: "c7".into(),
                type_name: Some("Person".into())
            })
        );
    }
}
