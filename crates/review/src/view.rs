//! The review tab (spec 008): what a branch changed since it forked from its target, folded per
//! entity, with fixes bound to the reviewed head and a merge that checks the head first.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use change_cards::{CardLines, CardRows, Row};
use editor::{Editor, EditorEvent};
use gpui::{
    AnyElement, App, AsyncApp, Context, Entity, EventEmitter, FocusHandle, Focusable,
    ListAlignment, ListState, Render, SharedString, Subscription, Task, WeakEntity, Window, list,
};
use gq_language::SchemaModel;
use graph_client::api::{
    BranchMergeOutcome, BranchMergeOutput, ChangeBlockOutput, ChangeCauseOutput, ChangeOutput,
    ChangeRequest, CommitListOutput, CommitOutput, MergeConflictOutput,
};
use graph_client::{ReadError, WriteOutcome};
use graph_store::fixes::revert_deletes;
use graph_store::{
    AccessAction, AccessScope, ChangeFold, EntityGroup, EntityGroups, FoldedChange, FoldedChanges,
    ForkSearch, GraphSession, GroupRun, GroupSection, HeadWatch, MergeSent, MergeSettled,
    NodeNames, OutcomeEntry, OutcomeKind, REVIEW_CHANGE_CAP, SessionEvent, TypeCounts,
    contains_merge_of, revert_query, summary_sentence,
};
use serde_json::Value;
use studio_actions::review::{
    AcceptAndNext, EditChange, Merge, NextChange, PreviousChange, RevertAndNext,
};
use studio_actions::workspace::Refresh;
use studio_ui::ds::{radii, space};
use studio_ui::{ChangeCard, change_counts, group_digits, short_commit_id, type_swatch};
use studio_workspace::{Item, ItemEvent, SavedItem, Workspace, workspace_for};
use ui::prelude::*;
use ui::{KeyBinding, PopoverMenu, Severity, Tooltip};

use crate::conflicts::{conflict_wording, resolution_hint};
use crate::scope_menu::ScopeMenu;

/// How long typing in the search pauses before the cards are filtered.
const SEARCH_DELAY: Duration = Duration::from_millis(80);

/// Changes asked for per change-feed page: the server's most (`COMMIT_CHANGES_MAX_CHANGES`),
/// as the CLI asks when it follows every page. Each page restarts the server's comparison
/// after the last change returned, so fewer, larger pages cost fewer reads; the server's byte
/// budget still bounds what one page holds.
const FEED_PAGE: usize = 8_192;
/// Commits asked for per page of a history: a branch's and its target's when the branch's
/// commits since the fork are read ([`commits_since_fork`]), a target's when a merge is looked
/// for in it ([`merge_shown`]). A page costs only the commits it returns; the server's most is
/// 1,000.
const FORK_PAGE: u32 = 500;
/// Pages of one history read, at most. Past them the fork, or a merge, is not looked for
/// further: the review shows what was read and says so.
const FORK_PAGES: usize = 20;
/// The most commits of one history a review reads.
const FORK_BOUND: usize = FORK_PAGE as usize * FORK_PAGES;

#[derive(Debug, Clone, PartialEq)]
enum FixState {
    Idle,
    Running,
    Reverted { commit: String },
    Saving,
    Edited { commit: String },
    Unchanged,
    Moved,
    Refused(String),
    Unknown,
}

impl FixState {
    /// A fix is out or has landed: the card shows the review as read, so the next fix waits for
    /// Refresh, from the buttons and the keys alike.
    fn done(&self) -> bool {
        matches!(
            self,
            Self::Running | Self::Saving | Self::Reverted { .. } | Self::Edited { .. }
        )
    }
}

#[derive(Debug, Clone)]
struct CardState {
    reviewed: bool,
    fix: FixState,
    /// Why Revert is unavailable, when it is.
    revert_blocked: Option<String>,
}

/// What reading a merge's target showed of the merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Check {
    /// The history is being read.
    Reading,
    Shown,
    /// The target's history, read to its end, does not show it.
    NotShown,
    /// The newest [`FORK_BOUND`] commits of the target's history do not show it, and the
    /// history goes on: Studio read no further, so it cannot tell.
    NotReached,
    /// The history could not be read, or the source head was not known: Studio cannot tell.
    Unread,
}

impl Check {
    /// What a read of a lost merge's target settles of it, for the session's log: a read that
    /// shows the merge or reaches past where it can be tells; one that stopped short does not.
    fn settled(self) -> Option<MergeSettled> {
        match self {
            Check::Shown => Some(MergeSettled::Landed),
            Check::NotShown => Some(MergeSettled::NotShown),
            Check::Reading | Check::NotReached | Check::Unread => None,
        }
    }
}

impl From<MergeSettled> for Check {
    fn from(settled: MergeSettled) -> Self {
        match settled {
            MergeSettled::Landed => Check::Shown,
            MergeSettled::NotShown => Check::NotShown,
        }
    }
}

#[derive(Debug, Clone)]
enum MergeState {
    Idle,
    Checking,
    Moved {
        head: Option<String>,
    },
    Merging,
    Merged {
        outcome: BranchMergeOutcome,
        commit: Option<String>,
        /// What the target's history shows of it.
        verified: Check,
        /// The merge took a head of the branch other than the one reviewed: someone committed
        /// to it between Studio's check and the merge.
        beyond_review: bool,
        /// The branch's deletion after the merge, when asked for: done, or why not.
        deleted: Option<Result<(), String>>,
    },
    Conflicts(Vec<MergeConflictOutput>),
    Refused(String),
    Unknown {
        reason: String,
        /// What the target's history shows of it.
        landed: Check,
        /// The heads the merge was sent over, which the target's history is read for. The
        /// session's outcome log keeps them with the merge's outcome, and what a read of the
        /// history showed of it: every review of these branches in the session starts from
        /// there ([`GraphSession::newest_merge`]).
        sent: MergeSent,
    },
}

/// The indexes of `changes` in order of type and id, for [`ReviewData::change_to`].
fn entity_order(changes: &[FoldedChange]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..changes.len()).collect();
    order.sort_by(|&a, &b| {
        (changes[a].type_name.as_str(), changes[a].id.as_str())
            .cmp(&(changes[b].type_name.as_str(), changes[b].id.as_str()))
    });
    order
}

impl ReviewData {
    /// The change to the entity of type `type_name` with `id`, if the branch changed it.
    fn change_to(&self, type_name: &str, id: &str) -> Option<usize> {
        let changes = &self.folded.changes;
        let key = |ix: usize| (changes[ix].type_name.as_str(), changes[ix].id.as_str());
        let found = self
            .by_entity
            .binary_search_by(|&ix| key(ix).cmp(&(type_name, id)))
            .ok()?;
        Some(self.by_entity[found])
    }
}

struct ReviewData {
    /// The merge base: the newest commit of the branch's history the target holds.
    fork: Option<String>,
    /// The branch head the review read (the reviewed head).
    head: Option<String>,
    /// The branch's commits since the fork, newest first: shared with the scope menu, which
    /// labels only the rows it draws.
    commits: Arc<[ReviewCommit]>,
    /// The merge base and the commits since the fork as they were read, for the heads they
    /// answer for: a scope change at the same heads takes them rather than reading them again.
    fork_read: Option<ForkRead>,
    /// Shared with the work done off the main thread: grouping again under another schema.
    folded: Arc<FoldedChanges>,
    /// The changes by entity: each node with the relationship changes reviewed on its card.
    groups: EntityGroups,
    /// Each change's search text ([`SearchIndex`]).
    search: SearchIndex,
    counts: Vec<TypeCounts>,
    /// Why each change cannot be reverted, when it cannot: worked out with the fold, off the
    /// main thread, since a review holds up to 20,000 changes.
    revert_blocked: Vec<Option<String>>,
    /// The changes' indexes in order of type and id, sorted with the fold: a conflict finds its
    /// entity's change by a search, not a walk of every change at each frame.
    by_entity: Vec<usize>,
    /// Each change's index by its key ([`FoldedChange::key`]), made with the fold: a fix's
    /// outcome finds its card without a walk of every change.
    by_key: HashMap<String, usize>,
    /// The cards' property rows, made as they are first drawn.
    card_rows: CardRows,
    /// Each change's reviewed mark ([`mark_of`]), made with the fold.
    marks: Vec<u64>,
    /// The schema the groups, the revert checks and the cards' rows were made with: one with
    /// another text, loaded since or applied, or naming nodes otherwise, makes them again.
    schema: Option<Arc<SchemaModel>>,
    partial: bool,
    /// The feed went past the reviewed head while reading: the branch moved.
    moved_while_reading: bool,
    /// The target was merged into the branch since it forked: `fork` is that merge's base, and
    /// the changes are the net ones since it, without the target's own.
    merged_in: bool,
    /// The fork lies past the commits read, [`FORK_BOUND`] of a history, and which history
    /// stopped the read: `commits` are the branch's own read, so their count and authors are
    /// not all known.
    commits_capped: Option<Capped>,
}

/// A fix asked for on a card: sent once the fixes before it are back.
struct Fix {
    /// The change's key, which finds its card when the outcome is back.
    key: String,
    request: ChangeRequest,
    description: String,
    edited: bool,
    /// The fix this one is the second half of: it is sent only once that one lands.
    after: Option<String>,
}

/// Why a fix is not sent while a merge is out, or its lost response is being settled.
const MERGE_RUNNING: &str =
    "A merge is running, or may be: a fix now could land on either side of it.";
const BRANCH_DELETED: &str = "The merge deleted the branch: there is no branch to fix now.";

/// What a reviewed mark is kept by: the entity and what its change does, so the mark holds
/// until the entity changes again, whichever commits the review reads the change from. A review
/// since a merge reads every change at the reviewed head, which a fix moves. Made with the fold,
/// off the main thread: a review holds up to 20,000 changes, and their values can be long.
fn mark_of(change: &FoldedChange) -> u64 {
    use std::fmt::Write as _;
    use std::hash::{DefaultHasher, Hash as _, Hasher};
    /// Hashes what is written to it, without making the text.
    struct Hashing<'a>(&'a mut DefaultHasher);
    impl std::fmt::Write for Hashing<'_> {
        fn write_str(&mut self, text: &str) -> std::fmt::Result {
            self.0.write(text.as_bytes());
            Ok(())
        }
    }
    let mut hasher = DefaultHasher::new();
    change.key().hash(&mut hasher);
    write!(
        Hashing(&mut hasher),
        "{:?}{:?}",
        change.before,
        change.after
    )
    .ok();
    hasher.finish()
}

/// A commit under review: from the commit list, or the cause of its change-feed block.
#[derive(Debug, Clone)]
pub(crate) struct ReviewCommit {
    pub(crate) id: String,
    pub(crate) actor_id: Option<String>,
    /// Unix microseconds.
    pub(crate) created_at: i64,
}

/// What a review read of the branch and its target that a review of another scope at the same
/// heads needs again: the merge base the server named for both heads and, once read, the
/// branch's commits since its fork.
#[derive(Debug, Clone)]
struct ForkRead {
    /// The merge base's commit.
    base: String,
    /// The branch's head and the target's, as the merge base answered for them.
    head: String,
    target_head: String,
    /// The branch's commits since the fork, once read ([`commits_since_fork`]), by whether the
    /// read started knowing the target was merged in (`[not knowing, knowing]`): that decides
    /// which pages it reads, so one started otherwise is not taken for it.
    since: [Option<Since>; 2],
}

impl ForkRead {
    /// The commits since the fork as read from a start knowing, or not, that the target was
    /// merged in.
    fn since(&self, merged_in: bool) -> Option<Since> {
        self.since[usize::from(merged_in)].clone()
    }

    fn keep_since(&mut self, merged_in: bool, since: Since) {
        self.since[usize::from(merged_in)] = Some(since);
    }
}

impl From<&CommitOutput> for ReviewCommit {
    fn from(commit: &CommitOutput) -> Self {
        Self {
            id: commit.graph_commit_id.clone(),
            actor_id: commit.actor_id.clone(),
            created_at: commit.created_at,
        }
    }
}

/// What a merge of the branch into its target would do now: a dry run read with the review,
/// which writes nothing.
#[derive(Debug, Clone)]
enum Preview {
    /// Not asked: a review of one commit, or the dry run could not be read.
    Unread,
    Reading,
    Merges(BranchMergeOutcome),
    Conflicts(Vec<MergeConflictOutput>),
}

enum LoadState {
    Loading { folded: usize },
    Failed(SharedString),
    Ready,
}

/// Why a list is not fixed from a review that does not show what the branch holds now.
const LIST_NEEDS_HEAD: &str = "A list is checked only against what the branch holds now, which \
     this review does not show: fix it from the review of all the branch's changes.";

/// Why an addition is not reverted from a review that does not show what the branch holds now
/// ([`revert_deletes`]).
const ADDITION_NEEDS_HEAD: &str = "Reverting an addition deletes it with whatever later commits \
     changed, and this review does not show what the branch holds now: revert it from the \
     review of all the branch's changes.";

/// The changes a search leaves, found off the main thread: each change's match for `query`,
/// none for an empty search, which leaves every card.
#[derive(Debug, Default)]
struct Searched {
    query: String,
    matched: Option<Vec<bool>>,
}

/// Each change's match for `query` (lowercase) in its search text; `None` for an empty query.
fn search_matches(texts: &[String], query: &str) -> Option<Vec<bool>> {
    (!query.is_empty()).then(|| texts.iter().map(|text| text.contains(query)).collect())
}

/// Each change's search text, lowercase ([`search_text_of`]), made the first time a search is
/// made, off the main thread: a search goes through these, not through every value of every
/// change on each key, and a review that is never searched never makes them.
#[derive(Clone, Default)]
struct SearchIndex {
    folded: Arc<FoldedChanges>,
    texts: Arc<std::sync::OnceLock<Vec<String>>>,
}

impl SearchIndex {
    fn new(folded: Arc<FoldedChanges>) -> Self {
        Self {
            folded,
            texts: Arc::default(),
        }
    }

    /// Which changes `query` matches, none asked when it is empty ([`search_matches`]).
    fn matches(&self, query: &str) -> Option<Vec<bool>> {
        if query.is_empty() {
            return None;
        }
        let texts = self
            .texts
            .get_or_init(|| self.folded.changes.iter().map(search_text_of).collect());
        search_matches(texts, query)
    }

    /// Whether `other` is this review's index, not one of a review read again since.
    fn is(&self, other: &SearchIndex) -> bool {
        Arc::ptr_eq(&self.texts, &other.texts)
    }
}

/// Why each change cannot be reverted under `schema`, when it cannot; nothing is said without
/// a schema, since a revert is made under the one loaded when it is asked for. A review that
/// does not show what the branch holds now (`shows_head`) reverts no addition.
fn revert_checks(
    folded: &FoldedChanges,
    schema: Option<&SchemaModel>,
    shows_head: bool,
) -> Vec<Option<String>> {
    folded
        .changes
        .iter()
        .map(|change| {
            let schema = schema?;
            match revert_query(schema, change) {
                Err(reason) => Some(reason.0),
                Ok(_) if !shows_head && revert_deletes(change) => {
                    Some(ADDITION_NEEDS_HEAD.to_owned())
                }
                Ok(_) => None,
            }
        })
        .collect()
}

/// Whether two schemas group and draw the cards alike: the same text, naming each type's nodes
/// by the same property. A schema read again with the same text is the same one.
fn same_schema(a: Option<&Arc<SchemaModel>>, b: Option<&Arc<SchemaModel>>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => Arc::ptr_eq(a, b) || a.same_as(b),
        (None, None) => true,
        _ => false,
    }
}

pub struct ReviewView {
    session: Entity<GraphSession>,
    branch: String,
    target: String,
    /// `None` reviews everything since the fork; `Some` one commit of the branch.
    scope: Option<String>,
    /// Narrows the cards to those whose id, type or values contain the text.
    search: Entity<Editor>,
    /// What the search left of the review shown.
    searched: Searched,
    focus_handle: FocusHandle,
    load: LoadState,
    data: Option<ReviewData>,
    /// Each change's state, by its index in the fold: a card's own change and each
    /// relationship line have one.
    cards: Vec<CardState>,
    rows: Vec<Row>,
    /// The runs of one type the rows show, each under its header.
    headers: Vec<GroupRun>,
    /// The selected change: an entity's own, or one relationship on its card.
    selected: Option<usize>,
    /// Names of the nodes the review knows only by id, read at the reviewed head after it
    /// loads; until then, and for a node without a name, its id shows.
    names: NodeNames,
    /// How many relationship lines each card shows.
    card_lines: CardLines,
    list_state: ListState,
    /// The conflicts list, drawn a screen at a time: the server's list has no bound.
    conflict_list: ListState,
    /// The head fixes and the merge bind to: the reviewed head, then each fix's own commit.
    bound_head: Option<String>,
    /// What the review holds, in a sentence, made when it loads: the header draws at every frame.
    summary: String,
    /// Reviewed marks, by entity and what its change does ([`mark_of`]), kept across Refresh:
    /// an entity changed again since is reviewed again.
    reviewed_keys: HashSet<u64>,
    merge: MergeState,
    /// Whether the session's log held a merge of these branches that may still land at the
    /// session's last event ([`Self::logged_merge_may_land`]): a hint to draw Merge and the fixes
    /// again when that changes, as another review's merge or read of the target's history does.
    /// Every decision reads the log itself.
    logged_merge_held: bool,
    /// The merge deleted the branch: the tab is not opened again with the window, whatever a
    /// Refresh since reads.
    branch_deleted: bool,
    /// What a merge would do now, read with the review by a dry run.
    preview: Preview,
    /// Whether the merge deletes the branch once it lands; on unless the person turns it off.
    delete_branch: bool,
    show_conflicts: bool,
    /// Someone else wrote to the branch after this review was read.
    newer: bool,
    /// Another writer changed the schema while the review being read was read: the review it
    /// finishes with is newer at once.
    schema_moved_while_loading: bool,
    /// Fixes asked for and not yet back, the one out and those waiting for it.
    fixes_running: usize,
    /// Fixes waiting for the one out: each is sent bound to the head the one before left.
    fix_queue: VecDeque<Fix>,
    fix_out: bool,
    /// What each fix asked for and not back yet shows, by its change's key: a Refresh while it
    /// is out keeps its card showing it, so the fix is not asked for a second time.
    fixes_in_flight: HashMap<String, FixState>,
    /// The session saw the branch's head move while a fix or the merge was out; compared once
    /// they are back, since each moves the bound head itself.
    head_moved_while_writing: bool,
    /// The session asks after the reviewed branch's head while the review is open.
    _head_watch: Option<HeadWatch>,
    _target_watch: HeadWatch,
    /// The dry run is for heads that moved (a fix landed, the target moved) and is read again
    /// once no fix or merge is out.
    preview_again: bool,
    /// The target moved since the dry run: it is read again at the next frame the review is
    /// drawn in ([`ReviewView::preview_moved`]), and a hidden review reads nothing.
    preview_owed: bool,
    /// The target's head as the session knew it when the dry run was asked for.
    preview_target: Option<String>,
    _load_task: Option<Task<()>>,
    _names_task: Option<Task<()>>,
    _preview_task: Option<Task<()>>,
    _merge_task: Option<Task<()>>,
    _search_task: Option<Task<()>>,
    /// The review grouped again under a schema that arrived since it was grouped.
    _regroup_task: Option<Task<()>>,
    /// The fix out: they go one at a time.
    _fix_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ItemEvent> for ReviewView {}

impl Focusable for ReviewView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for ReviewView {
    fn tab_title(&self, _cx: &App) -> SharedString {
        format!("Review {} → {}", self.branch, self.target).into()
    }

    fn tab_icon(&self, _cx: &App) -> Option<IconName> {
        Some(IconName::GitBranch)
    }

    fn dedupe_key(&self, _cx: &App) -> Option<SharedString> {
        Some(review_key(&self.branch, &self.target).into())
    }

    fn serialize(&self, _cx: &App) -> Option<SavedItem> {
        (!self.branch_deleted).then(|| SavedItem {
            kind: "review".into(),
            data: serde_json::json!({ "branch": self.branch, "target": self.target }),
        })
    }

    fn is_stale(&self, _cx: &App) -> bool {
        self.newer || self.bound_head != self.reviewed_head()
    }
}

/// The dedupe key of a review tab.
pub fn review_key(branch: &str, target: &str) -> String {
    format!("review:{branch}->{target}")
}

impl ReviewView {
    pub fn new(
        session: Entity<GraphSession>,
        branch: String,
        target: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Find a change by id, type or value", window, cx);
            editor
        });
        let search_subscription = cx.subscribe(&search, |this, _, event: &EditorEvent, cx| {
            if let EditorEvent::BufferEdited = event {
                // Once typing pauses: each search goes through every change.
                this.search_after(Some(SEARCH_DELAY), cx);
            }
        });
        let subscription =
            cx.subscribe(&session, |this, _, event: &SessionEvent, cx| match event {
                SessionEvent::HeadMoved { branch, .. } if *branch == this.branch => {
                    this.compare_head(cx)
                }
                // The dry run merged into the target as it was. It is read again when the review
                // is next drawn: a review behind another tab asks the server nothing meanwhile.
                SessionEvent::HeadMoved { branch, head }
                    if *branch == this.target
                        && this.preview_target.as_deref() != Some(head.as_str()) =>
                {
                    this.preview_owed = true;
                    cx.notify();
                }
                SessionEvent::SchemaChanged => this.schema_changed(cx),
                SessionEvent::SchemaMoved => this.schema_moved(cx),
                SessionEvent::CredentialChanged => this.credential_changed(cx),
                SessionEvent::ActivityChanged => this.activity_changed(cx),
                SessionEvent::BranchesChanged => this.branches_changed(cx),
                // Merge, and deleting the branch with it, follow what the credential may do. An
                // answer dropped (a 403, a new token) is read again; one held asks nothing.
                SessionEvent::AccessChanged => {
                    let scope = AccessScope::branch_into(&this.branch, &this.target);
                    this.session
                        .update(cx, |session, cx| session.read_access(scope, cx));
                    cx.notify();
                }
                _ => {}
            });
        let head_watch = session.update(cx, |session, cx| session.watch_head(&branch, cx));
        // What the credential may do with the branch and its target, merging included: read
        // once per credential, so a review opened again asks nothing.
        session.update(cx, |session, cx| {
            session.read_access(AccessScope::branch_into(&branch, &target), cx)
        });
        let target_watch = session.update(cx, |session, cx| session.watch_head(&target, cx));
        let mut view = Self {
            session,
            branch,
            target,
            scope: None,
            search,
            searched: Searched::default(),
            focus_handle: cx.focus_handle(),
            load: LoadState::Loading { folded: 0 },
            data: None,
            cards: Vec::new(),
            rows: Vec::new(),
            headers: Vec::new(),
            selected: None,
            names: NodeNames::default(),
            card_lines: CardLines::default(),
            list_state: ListState::new(0, ListAlignment::Top, px(800.)),
            conflict_list: ListState::new(0, ListAlignment::Top, px(800.)),
            bound_head: None,
            summary: String::new(),
            reviewed_keys: HashSet::new(),
            merge: MergeState::Idle,
            logged_merge_held: false,
            branch_deleted: false,
            preview: Preview::Unread,
            delete_branch: true,
            show_conflicts: false,
            newer: false,
            schema_moved_while_loading: false,
            fixes_running: 0,
            fix_queue: VecDeque::new(),
            fix_out: false,
            fixes_in_flight: HashMap::new(),
            head_moved_while_writing: false,
            _head_watch: Some(head_watch),
            _target_watch: target_watch,
            preview_again: false,
            preview_owed: false,
            preview_target: None,
            _load_task: None,
            _names_task: None,
            _preview_task: None,
            _merge_task: None,
            _search_task: None,
            _regroup_task: None,
            _fix_task: None,
            _subscriptions: vec![subscription, search_subscription],
        };
        // Read as any Refresh is: a merge of these branches whose response was lost and may
        // still land, sent from a review closed since or open elsewhere, is where this one
        // starts, from the session's log.
        view.reload(cx);
        view.logged_merge_held = view.logged_merge_may_land(cx);
        view
    }

    /// The session logged an outcome, recorded what a target's history showed of a lost merge,
    /// or sent or answered a read. What another review of these branches recorded for the lost
    /// merge this one holds, and could not tell of, is this one's too. Merge and the fixes follow
    /// the log's newest merge of these branches, so the review draws again when what it says
    /// changes, and a fix refused while it might land may be asked for again.
    fn activity_changed(&mut self, cx: &mut Context<Self>) {
        let mut settled = false;
        if let MergeState::Unknown {
            landed: landed @ (Check::NotReached | Check::Unread),
            sent,
            ..
        } = &mut self.merge
            && let Some(logged) = self
                .session
                .read(cx)
                .merge_sent(&self.branch, &self.target, sent)
                .and_then(|merge| merge.settled)
        {
            *landed = logged.into();
            settled = true;
        }
        let held = self.logged_merge_may_land(cx);
        if settled || held != self.logged_merge_held {
            self.logged_merge_held = held;
            self.offer_fixes_again(cx);
            if settled {
                cx.emit(ItemEvent::UpdateTab);
            }
            cx.notify();
        }
    }

    /// The branch list was read. A reviewed branch it no longer holds was deleted: by the delete
    /// of a merge whose answer was lost and that landed, or outside Studio. Like one the merge
    /// said it deleted, its head is not watched, nothing is fixed on it, and its review is not
    /// opened again with the window.
    fn branches_changed(&mut self, cx: &mut Context<Self>) {
        if self.branch_deleted {
            return;
        }
        let gone = self
            .session
            .read(cx)
            .branches
            .loaded()
            .is_some_and(|branches| !branches.contains(&self.branch));
        if gone {
            self._head_watch = None;
            self.branch_deleted = true;
            cx.emit(ItemEvent::UpdateTab);
            cx.notify();
        }
    }

    /// The session's schema was loaded, applied, or names nodes otherwise: the review shown is
    /// grouped again under it. One being read compares its schema once it is in.
    fn schema_changed(&mut self, cx: &mut Context<Self>) {
        if matches!(self.load, LoadState::Ready) {
            self.regroup(cx);
        }
        cx.notify();
    }

    /// The session reads under a new token, which may see less than the one the cards, their
    /// names and the dry run were read with: none of them shows while the review is read again
    /// under it, the names included, which a Refresh otherwise keeps.
    ///
    /// Everything the old token read goes: the cards, the counts, the banners and the scope
    /// menu's commits with the review it read, and the merge base and commits since the fork a
    /// scope change would take. A read the new token is refused leaves none of it. Reviewed
    /// marks are the person's and stay. A fix asked for and not back is still sent, bound to the
    /// head the one before it left, and the server judges it under the new token.
    fn credential_changed(&mut self, cx: &mut Context<Self>) {
        self.keep_marks();
        self.data = None;
        self.cards.clear();
        self.rows.clear();
        self.headers.clear();
        self.list_state.reset(0);
        self.selected = None;
        self.searched = Searched::default();
        self.summary.clear();
        self._search_task = None;
        self._regroup_task = None;
        if self.fixes_running == 0 {
            self.bound_head = None;
        }
        self.names.clear();
        self._names_task = None;
        self.preview = Preview::Unread;
        self._preview_task = None;
        self.reload(cx);
    }

    /// Another writer's schema: the cards were made under the one before, as are those of a
    /// review being read, which keeps it for when it finishes.
    fn schema_moved(&mut self, cx: &mut Context<Self>) {
        if matches!(self.load, LoadState::Loading { .. }) {
            self.schema_moved_while_loading = true;
        }
        if !self.newer {
            self.mark_newer(cx);
        }
    }

    /// Groups the review again when it was grouped under another schema than the session's,
    /// off the main thread: its cards pair moved relationships, say what cannot be reverted and
    /// show their rows by the schema they are drawn with. A tab opened again with the window can
    /// finish reading before the schema loads. The changes stay as read: a schema changes how
    /// they are grouped, not what the branch changed.
    fn regroup(&mut self, cx: &mut Context<Self>) {
        let schema = self.session.read(cx).schema_model();
        let shows_head = self.shows_head();
        let Some(data) = &self.data else { return };
        if same_schema(data.schema.as_ref(), schema.as_ref()) {
            // Grouped as the session's schema groups: a regroup under way is for one since gone.
            self._regroup_task = None;
            return;
        }
        let folded = data.folded.clone();
        self._regroup_task = Some(cx.spawn(async move |this, cx| {
            let (grouping, made_with) = (folded.clone(), schema.clone());
            let (groups, revert_blocked) = cx
                .background_executor()
                .spawn(async move {
                    let groups = EntityGroups::new(&grouping, schema.as_deref());
                    let blocked = revert_checks(&grouping, schema.as_deref(), shows_head);
                    (groups, blocked)
                })
                .await;
            this.update(cx, |this, cx| {
                // Grouped for a review read again since: that one compares its own schema.
                let Some(data) = this
                    .data
                    .as_mut()
                    .filter(|data| Arc::ptr_eq(&data.folded, &folded))
                else {
                    return;
                };
                data.groups = groups;
                data.schema = made_with;
                data.card_rows = CardRows::default();
                for (card, blocked) in this.cards.iter_mut().zip(&revert_blocked) {
                    card.revert_blocked = blocked.clone();
                }
                data.revert_blocked = revert_blocked;
                let ends = data.groups.unnamed_ends();
                let head = data.head.clone();
                this.lay_out_in_place();
                this.read_names(ends, head, cx);
                cx.notify();
            })
            .ok();
        }));
    }

    fn reviewed_head(&self) -> Option<String> {
        self.data.as_ref().and_then(|d| d.head.clone())
    }

    /// Keeps the cards' reviewed marks for the review read next ([`mark_of`]).
    fn keep_marks(&mut self) {
        for (state, mark) in self
            .cards
            .iter()
            .zip(self.data.iter().flat_map(|d| d.marks.iter()))
        {
            if state.reviewed {
                self.reviewed_keys.insert(*mark);
            } else {
                self.reviewed_keys.remove(mark);
            }
        }
    }

    /// Reads the review again from the branch head. Reviewed marks are kept.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.read_again(None, cx);
    }

    /// Reads the review again: from the merge base and the commits since the fork in `held`,
    /// read for heads the branch and its target still stand at, else from the branch head.
    fn read_again(&mut self, held: Option<ForkRead>, cx: &mut Context<Self>) {
        self.keep_marks();
        self.load = LoadState::Loading { folded: 0 };
        self.newer = false;
        self.schema_moved_while_loading = false;
        self.show_conflicts = false;
        // A merge out keeps its state and its task. One whose lost response the target's
        // history did not settle is settled by reading that history again, never by merging
        // again (constitution III); so is the newest merge of these branches the session's log
        // holds, sent by a review closed since or open elsewhere, when it may still land.
        if !self.merging() {
            if self.merge_unsettled() {
                self.settle_merge(cx);
            } else if let Some((reason, sent)) = self.logged_lost_merge(cx) {
                self.merge = MergeState::Unknown {
                    reason,
                    landed: Check::Reading,
                    sent,
                };
                self.settle_merge(cx);
            } else {
                self.merge = MergeState::Idle;
            }
        }
        let session = self.session.clone();
        let branch = self.branch.clone();
        let target = self.target.clone();
        let scope = self.scope.clone();
        self._load_task = Some(cx.spawn(async move |this, cx| {
            let result = read_review(session, branch, target, scope, held, this.clone(), cx).await;
            // The search's matches among the changes read, found off the main thread too.
            let searched = match &result {
                Ok(data) => {
                    let query = this
                        .update(cx, |this, cx| this.search_text(cx))
                        .unwrap_or_default();
                    let (texts, text) = (data.search.clone(), query.clone());
                    let matched = cx
                        .background_executor()
                        .spawn(async move { texts.matches(&text) })
                        .await;
                    Searched { query, matched }
                }
                Err(_) => Searched::default(),
            };
            this.update(cx, |this, cx| this.finish_load(result, searched, cx))
                .ok();
        }));
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    fn finish_load(
        &mut self,
        result: Result<ReviewData, String>,
        searched: Searched,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(data) => {
                self.cards = data
                    .revert_blocked
                    .iter()
                    .zip(&data.marks)
                    .map(|(blocked, mark)| CardState {
                        reviewed: self.reviewed_keys.contains(mark),
                        fix: FixState::Idle,
                        revert_blocked: blocked.clone(),
                    })
                    .collect();
                // A fix still out keeps its card showing it, so it is not asked for again.
                for (key, fix) in &self.fixes_in_flight {
                    if let Some(&ix) = data.by_key.get(key) {
                        self.cards[ix].fix = fix.clone();
                    }
                }
                self.bound_head = data.head.clone();
                let schema_moved = std::mem::take(&mut self.schema_moved_while_loading);
                self.newer = data.moved_while_reading || schema_moved;
                self.summary = self.summary_of(&data);
                let ends = data.groups.unnamed_ends();
                let head = data.head.clone();
                let changed = !data.folded.changes.is_empty();
                // Typed while the changes were gone through: gone through again for the text.
                let typed_since = searched.query != self.search_text(cx);
                self.searched = searched;
                self.data = Some(data);
                self.load = LoadState::Ready;
                self.selected = None;
                self.lay_out();
                if typed_since {
                    self.search_after(None, cx);
                }
                self.read_names(ends, head, cx);
                self.preview = Preview::Unread;
                self._preview_task = None;
                // A head that moved while this was read, a fix of this review's included, is
                // offered as newer: the cards show the head read. Compared before the dry run,
                // which is not read for a head the cards no longer show.
                self.compare_head(cx);
                // Read now, or once the fix or merge out is back.
                if changed {
                    self.preview_moved(cx);
                }
                // A schema that arrived while this was read groups it again.
                self.regroup(cx);
            }
            Err(error) => self.load = LoadState::Failed(error.into()),
        }
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    /// Reads the names of the nodes the review knows only by id, at the head it read. A name
    /// already known is not read again: a node is known only by id while the branch has not
    /// changed it, so its name has not moved, and a Refresh keeps showing it.
    fn read_names(
        &mut self,
        ends: Vec<(String, String)>,
        head: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let missing: Vec<_> = ends
            .into_iter()
            .filter(|end| !self.names.contains_key(end))
            .collect();
        if missing.is_empty() {
            self._names_task = None;
            return;
        }
        let names = self
            .session
            .update(cx, |session, cx| session.node_names(missing, head, cx));
        self._names_task = Some(cx.spawn(async move |this, cx| {
            let names = names.await;
            this.update(cx, |this, cx| {
                this.names.extend(names);
                cx.notify();
            })
            .ok();
        }));
    }

    /// Asks what a merge would do now, by a dry run: the branch reads as merging cleanly, or
    /// its conflicts show before anyone merges. A dry run that cannot be read says nothing.
    fn read_preview(&mut self, cx: &mut Context<Self>) {
        self.preview = Preview::Reading;
        self.preview_target = self.session.read(cx).head(&self.target).map(str::to_owned);
        let read = self.session.update(cx, |session, cx| {
            session.merge_preview(&self.branch, &self.target, cx)
        });
        self._preview_task = Some(cx.spawn(async move |this, cx| {
            let preview = match read.await {
                Ok(graph_client::MergePreview::Merges(outcome)) => Preview::Merges(outcome),
                Ok(graph_client::MergePreview::Conflicts(conflicts)) => {
                    Preview::Conflicts(conflicts)
                }
                Err(_) => Preview::Unread,
            };
            this.update(cx, |this, cx| {
                this.preview = preview;
                cx.notify();
            })
            .ok();
        }));
    }

    /// The heads the dry run merged moved: it is read again for them, once no fix or merge is
    /// out; a review being read reads its own.
    fn preview_moved(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.load, LoadState::Ready) {
            return;
        }
        // The cards show a head another writer has moved past: a dry run now would merge
        // changes they do not show. Refresh reads it with the review.
        if self.newer {
            self.preview_again = false;
            self.preview = Preview::Unread;
            self._preview_task = None;
            return;
        }
        if self.fixes_running > 0 || self.merging() {
            self.preview_again = true;
            return;
        }
        self.preview_again = false;
        // A merge's own outcome shows in its place.
        if !matches!(self.merge, MergeState::Idle) {
            return;
        }
        let changed = self
            .data
            .as_ref()
            .is_some_and(|data| !data.folded.changes.is_empty());
        // A merge the credential may not make is not previewed: the dry run asks the server
        // what that merge would do.
        if self.scope.is_none() && changed && self.merge_refusal(cx).is_none() {
            self.read_preview(cx);
        } else {
            self.preview = Preview::Unread;
        }
    }

    /// The conflicts the conflicts view lists: a merge's, else a dry run's.
    fn conflicts(&self) -> Option<&[MergeConflictOutput]> {
        match (&self.merge, &self.preview) {
            (MergeState::Conflicts(conflicts), _) => Some(conflicts),
            (MergeState::Idle, Preview::Conflicts(conflicts)) => Some(conflicts),
            _ => None,
        }
    }

    fn change_count(&self) -> usize {
        self.data.as_ref().map_or(0, |d| d.folded.changes.len())
    }

    /// The group whose card a change is reviewed on.
    fn group_of(&self, change: usize) -> Option<usize> {
        self.data.as_ref()?.groups.home.get(change).copied()
    }

    fn row_of_group(&self, group: usize) -> Option<usize> {
        self.rows
            .iter()
            .position(|row| matches!(row, Row::Entity(ix) if *ix == group))
    }

    /// Selects a change, on its entity's card, and scrolls the card into view.
    fn select(&mut self, change: usize, cx: &mut Context<Self>) {
        if change < self.change_count() {
            self.selected = Some(change);
            if let Some(row) = self.group_of(change).and_then(|g| self.row_of_group(g)) {
                self.list_state.scroll_to_reveal_item(row);
            }
            cx.notify();
        }
    }

    /// The groups the search leaves, in list order.
    fn visible_groups(&self) -> Vec<usize> {
        self.rows
            .iter()
            .filter_map(|row| match row {
                Row::Entity(ix) => Some(*ix),
                Row::Header(_) => None,
            })
            .collect()
    }

    /// Moves to the next or previous entity, selecting its own change (or its first
    /// relationship's).
    fn step(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(data) = &self.data else { return };
        let visible = self.visible_groups();
        let Some(&first) = visible.first() else {
            return;
        };
        let next = match self
            .selected
            .and_then(|s| data.groups.home.get(s))
            .and_then(|g| visible.iter().position(|ix| ix == g))
        {
            Some(at) if forward => visible[(at + 1).min(visible.len() - 1)],
            Some(at) => visible[at.saturating_sub(1)],
            None => first,
        };
        if let Some(change) = data.groups.groups[next].primary() {
            self.select(change, cx);
        }
    }

    fn select_next(&mut self, _: &NextChange, _: &mut Window, cx: &mut Context<Self>) {
        self.step(true, cx);
    }

    fn select_previous(&mut self, _: &PreviousChange, _: &mut Window, cx: &mut Context<Self>) {
        self.step(false, cx);
    }

    fn search_text(&self, cx: &App) -> String {
        self.search.read(cx).text(cx).trim().to_lowercase()
    }

    /// Shows only the cards matching the search, once typing has paused for `delay`: the
    /// changes' texts are gone through off the main thread, since a review holds up to 20,000
    /// changes. The latest search wins, and one made for a review read again since is let go.
    fn search_after(&mut self, delay: Option<Duration>, cx: &mut Context<Self>) {
        self._search_task = Some(cx.spawn(async move |this, cx| {
            if let Some(delay) = delay {
                cx.background_executor().timer(delay).await;
            }
            let Ok(Some((texts, query))) = this.update(cx, |this, cx| {
                let texts = this.data.as_ref()?.search.clone();
                Some((texts, this.search_text(cx)))
            }) else {
                return;
            };
            let (scanned, text) = (texts.clone(), query.clone());
            let matched = cx
                .background_executor()
                .spawn(async move { scanned.matches(&text) })
                .await;
            this.update(cx, |this, cx| {
                if this
                    .data
                    .as_ref()
                    .is_some_and(|data| data.search.is(&texts))
                {
                    this.searched = Searched { query, matched };
                    this.lay_out();
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    /// Lays the cards out as the search leaves them, keeping the selection while its card
    /// shows, else selecting the first card shown.
    fn lay_out(&mut self) {
        let Some(data) = &self.data else { return };
        (self.rows, self.headers) = build_rows(data, self.searched.matched.as_deref());
        self.list_state.reset(self.rows.len());
        let visible = self.visible_groups();
        let shown = self
            .selected
            .and_then(|s| data.groups.home.get(s))
            .is_some_and(|g| visible.contains(g));
        if !shown {
            self.selected = visible
                .first()
                .and_then(|&group| data.groups.groups[group].primary());
        }
    }

    /// [`Self::lay_out`] for the same changes grouped again, keeping the place the person
    /// scrolled to.
    fn lay_out_in_place(&mut self) {
        let top = self.list_state.logical_scroll_top();
        self.lay_out();
        self.list_state.scroll_to(top);
    }

    /// What the scope menu lists, the review's own commits shared, and the scope shown.
    pub(crate) fn scope_menu_input(&self) -> (Arc<[ReviewCommit]>, Option<String>) {
        let commits = self
            .data
            .as_ref()
            .map_or_else(|| Vec::new().into(), |data| data.commits.clone());
        (commits, self.scope.clone())
    }

    /// Reviews everything since the fork (`None`) or one commit, reading again. The merge base
    /// and the commits since the fork read for the heads the review shows are taken as held
    /// while those heads stand: a scope change reads only what the new scope shows.
    pub(crate) fn set_scope(&mut self, scope: Option<String>, cx: &mut Context<Self>) {
        if self.scope != scope {
            let held = self.held_fork_read(cx);
            self.scope = scope;
            self.read_again(held, cx);
        }
    }

    /// The merge base and the commits since the fork the review read, while the heads they
    /// answer for stand as far as Studio knows: no other writer's commit or schema seen on the
    /// branch since (`newer`), no fix of this review's landed, and the session seeing neither
    /// branch at another commit. A head that moved unseen is offered as newer once seen, as for
    /// any review.
    fn held_fork_read(&self, cx: &App) -> Option<ForkRead> {
        let read = self.data.as_ref()?.fork_read.as_ref()?;
        let session = self.session.read(cx);
        let moved =
            |branch: &str, head: &str| session.head(branch).is_some_and(|seen| seen != head);
        let stands = !self.newer
            && self.bound_head.as_deref() == Some(read.head.as_str())
            && !moved(&self.branch, &read.head)
            && !moved(&self.target, &read.target_head);
        stands.then(|| read.clone())
    }

    /// Whether the keys may act on the cards: they are on screen. A review being read again
    /// keeps the cards it read before, under "Reading changes…", an error or the conflicts list,
    /// and a key there would revert or mark a card nobody sees.
    fn cards_shown(&self) -> bool {
        matches!(self.load, LoadState::Ready) && !self.show_conflicts
    }

    /// Marks the selected entity's card reviewed, every change on it, and moves to the next.
    fn accept_and_next(&mut self, _: &AcceptAndNext, window: &mut Window, cx: &mut Context<Self>) {
        if !self.cards_shown() {
            return;
        }
        if let Some(group) = self.selected.and_then(|s| self.group_of(s)) {
            self.set_group_reviewed(group, true);
            self.select_next(&NextChange, window, cx);
        }
    }

    fn revert_and_next(&mut self, _: &RevertAndNext, window: &mut Window, cx: &mut Context<Self>) {
        if !self.cards_shown() {
            return;
        }
        if let Some(ix) = self.selected {
            self.revert_line(ix, cx);
            self.select_next(&NextChange, window, cx);
        }
    }

    /// Reverts a change as its card shows it: a relationship moved to another end
    /// ([`EntityGroup::repoints`]) moves back, the new edge removed before the old one returns,
    /// since an edge type with one target per source holds one at a time. Fixes go one at a
    /// time, so the second waits for the first, and is sent only once the first lands: both are
    /// made before either is sent, so the new edge is never removed with the old one not coming
    /// back.
    fn revert_line(&mut self, change: usize, cx: &mut Context<Self>) {
        let pair = self.group_of(change).and_then(|group| {
            let data = self.data.as_ref()?;
            data.groups.groups[group]
                .repoints
                .iter()
                .copied()
                .find(|&(removed, added)| removed == change || added == change)
        });
        match pair {
            Some((removed, added)) => {
                let (Some(remove), Some(mut restore)) =
                    (self.revert_fix(added, cx), self.revert_fix(removed, cx))
                else {
                    return;
                };
                restore.after = Some(remove.key.clone());
                for (ix, fix) in [(added, remove), (removed, restore)] {
                    self.cards[ix].fix = FixState::Running;
                    self.queue_fix(fix, cx);
                }
            }
            None => self.revert(change, cx),
        }
    }

    /// How many changes on a group's card are not reviewed.
    fn group_unreviewed(&self, group: &EntityGroup) -> usize {
        group
            .changes()
            .filter(|&c| !self.cards.get(c).is_some_and(|card| card.reviewed))
            .count()
    }

    fn set_group_reviewed(&mut self, group: usize, reviewed: bool) {
        let Some(data) = &self.data else { return };
        for change in data.groups.groups[group].changes() {
            if let Some(card) = self.cards.get_mut(change) {
                card.reviewed = reviewed;
            }
        }
    }

    /// Marks a card reviewed, or not: every change on it at once.
    fn toggle_reviewed(&mut self, group: usize, cx: &mut Context<Self>) {
        let Some(data) = &self.data else { return };
        let all = self.group_unreviewed(&data.groups.groups[group]) == 0;
        self.set_group_reviewed(group, !all);
        cx.notify();
    }

    /// Restores a change's before image on the branch, bound to the head this review holds.
    /// Never retried; the card shows the outcome.
    fn revert(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(fix) = self.revert_fix(ix, cx) {
            self.cards[ix].fix = FixState::Running;
            self.queue_fix(fix, cx);
        }
    }

    /// The fix that restores change `ix`'s before image, not sent yet; `None` when it cannot be
    /// made, with the card saying why.
    fn revert_fix(&mut self, ix: usize, cx: &mut Context<Self>) -> Option<Fix> {
        let data = self.data.as_ref()?;
        let change = data.folded.changes.get(ix)?;
        // As the buttons are: disabled, saying why, while the credential may not change the
        // branch.
        if self.cards.get(ix)?.fix.done() || self.change_refusal(cx).is_some() {
            return None;
        }
        if self.merge_may_land(cx) || self.branch_deleted {
            let reason = if self.branch_deleted {
                BRANCH_DELETED
            } else {
                MERGE_RUNNING
            };
            self.cards[ix].fix = FixState::Refused(reason.into());
            cx.notify();
            return None;
        }
        let Some(schema) = self.session.read(cx).schema_model() else {
            self.cards[ix].fix = FixState::Refused("The schema has not loaded yet.".into());
            cx.notify();
            return None;
        };
        let query = match revert_query(&schema, change) {
            Ok(query) => query,
            Err(reason) => {
                self.cards[ix].revert_blocked = Some(reason.0);
                cx.notify();
                return None;
            }
        };
        let lists = change.after.as_ref().is_some_and(|after| {
            schema.node(&change.type_name).is_some_and(|node| {
                node.properties.iter().any(|property| {
                    property.list
                        && change
                            .before
                            .as_ref()
                            .and_then(|before| before.properties.get(&property.name))
                            != after.properties.get(&property.name)
                })
            })
        });
        if lists && !self.shows_head() {
            self.cards[ix].revert_blocked = Some(LIST_NEEDS_HEAD.into());
            cx.notify();
            return None;
        }
        if revert_deletes(change) && !self.shows_head() {
            self.cards[ix].revert_blocked = Some(ADDITION_NEEDS_HEAD.into());
            cx.notify();
            return None;
        }
        let request = ChangeRequest {
            query: query.text.clone(),
            name: Some(query.name.clone()),
            params: query.params_value(),
            branch: Some(self.branch.clone()),
        };
        Some(Fix {
            key: change.key(),
            request,
            description: format!("Revert {} {}", change.type_name, change.id),
            edited: false,
            after: None,
        })
    }

    /// A merge is out: its check, its request, or the read of the target's history that settles
    /// a response that was lost. A fix now could land on either side of it, and a second merge
    /// would cancel that read and be sent while the first may still land.
    fn merging(&self) -> bool {
        matches!(
            self.merge,
            MergeState::Checking
                | MergeState::Merging
                | MergeState::Unknown {
                    landed: Check::Reading,
                    ..
                }
        )
    }

    /// A merge whose response was lost and which the target's history read did not settle: it
    /// did not reach far enough ([`Check::NotReached`]) or could not be read ([`Check::Unread`]).
    /// The merge may have landed, or still land, so Merge waits for a read that tells, which a
    /// Refresh asks for.
    fn merge_unsettled(&self) -> bool {
        matches!(
            self.merge,
            MergeState::Unknown {
                landed: Check::NotReached | Check::Unread,
                ..
            }
        )
    }

    /// A merge is out, or its lost response is not settled, this review's or another's of these
    /// branches: either may still land, so a fix now could land on either side of it, and the
    /// fixes wait, as Merge does.
    fn merge_may_land(&self, cx: &App) -> bool {
        self.merging() || self.merge_unsettled() || self.logged_merge_may_land(cx)
    }

    /// Whether the newest merge of these branches the session's log holds may still land: its
    /// outcome is unknown and no read of the target's history has settled it, whichever review
    /// sent it, open still or closed since.
    fn logged_merge_may_land(&self, cx: &App) -> bool {
        self.session
            .read(cx)
            .newest_merge(&self.branch, &self.target)
            .is_some_and(OutcomeEntry::merge_may_land)
    }

    /// That merge, when it may still land: why its outcome is unknown, and the heads it was
    /// sent over, which the target's history is read for.
    fn logged_lost_merge(&self, cx: &App) -> Option<(String, MergeSent)> {
        let entry = self
            .session
            .read(cx)
            .newest_merge(&self.branch, &self.target)
            .filter(|entry| entry.merge_may_land())?;
        let OutcomeKind::Unknown { reason } = &entry.outcome else {
            return None;
        };
        Some((reason.clone(), entry.merge.as_ref()?.sent.clone()))
    }

    /// Takes what a read of the target's history showed of this review's lost merge `sent`. A
    /// read that tells is recorded in the session's log, where every review of these branches
    /// reads it; one that cannot tell takes what another review's read recorded there
    /// meanwhile. A merge shown to have landed stays so.
    fn lost_merge_read(
        &mut self,
        reason: String,
        read: Check,
        sent: MergeSent,
        cx: &mut Context<Self>,
    ) {
        let (branch, target) = (self.branch.clone(), self.target.clone());
        let logged = self.session.update(cx, |session, cx| match read.settled() {
            Some(settled) => session.settle_merge(&branch, &target, &sent, settled, cx),
            None => session
                .merge_sent(&branch, &target, &sent)
                .and_then(|merge| merge.settled),
        });
        self.merge = MergeState::Unknown {
            reason,
            landed: logged.map_or(read, Check::from),
            sent,
        };
    }

    /// Reads the target's history again for a merge whose response was lost, which the history
    /// read before did not settle: never by merging again (constitution III).
    fn settle_merge(&mut self, cx: &mut Context<Self>) {
        let MergeState::Unknown { reason, sent, .. } = &self.merge else {
            return;
        };
        let (reason, sent) = (reason.clone(), sent.clone());
        self.merge = MergeState::Unknown {
            reason: reason.clone(),
            landed: Check::Reading,
            sent: sent.clone(),
        };
        let session = self.session.clone();
        let (branch, target) = (self.branch.clone(), self.target.clone());
        self._merge_task = Some(cx.spawn(async move |this, cx| {
            let landed = merge_landed(&session, &branch, &target, &sent, cx).await;
            this.update(cx, |this, cx| {
                // The merge may have deleted the branch: the session reads the branches again.
                this.session.update(cx, |s, cx| s.reload_branches(cx));
                this.lost_merge_read(reason, landed, sent, cx);
                this.writes_settled(cx);
                cx.emit(ItemEvent::UpdateTab);
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Why the credential may not change the branch, once the access route said so (spec 002),
    /// from the answer the review reads for the branch and its target, or one read for the
    /// branch alone: Revert and Edit are disabled with it as their reason, and their keys are
    /// not registered.
    fn change_refusal(&self, cx: &App) -> Option<String> {
        let session = self.session.read(cx);
        session
            .refusal(
                AccessAction::Change,
                &AccessScope::branch_into(&self.branch, &self.target),
            )
            .or_else(|| session.refusal(AccessAction::Change, &AccessScope::branch(&self.branch)))
    }

    /// Whether every card shows its entity as the reviewed head holds it: not so for one commit
    /// of the branch, nor for a review cut short. A fix is bound to that head; a list, which a
    /// write's `where` cannot compare, and a delete, bound to no value, are safe to send only
    /// from what it holds.
    fn shows_head(&self) -> bool {
        self.scope.is_none() && self.data.as_ref().is_some_and(|data| !data.partial)
    }

    /// Sends `fix` once the fixes before it are back. Each is bound to the head the one before
    /// left, so a second fix asked for quickly is not refused as "moved" by the first.
    fn queue_fix(&mut self, fix: Fix, cx: &mut Context<Self>) {
        self.fixes_running += 1;
        let showing = if fix.edited {
            FixState::Saving
        } else {
            FixState::Running
        };
        self.fixes_in_flight.insert(fix.key.clone(), showing);
        self.fix_queue.push_back(fix);
        self.send_next_fix(cx);
        cx.notify();
    }

    fn send_next_fix(&mut self, cx: &mut Context<Self>) {
        if self.fix_out {
            return;
        }
        let Some(Fix {
            key,
            request,
            description,
            edited,
            ..
        }) = self.fix_queue.pop_front()
        else {
            return;
        };
        self.fix_out = true;
        let if_commit = self.bound_head.clone();
        let task = self.session.update(cx, |session, cx| {
            session.write_request(request, if_commit, description, cx)
        });
        self._fix_task = Some(cx.spawn(async move |this, cx| {
            let outcome = task.await;
            this.update(cx, |this, cx| this.finish_fix(&key, outcome, edited, cx))
                .ok();
        }));
    }

    /// The editable properties of a node change, with what the branch holds now; `None` for
    /// a relationship, a removed node, or a type the schema no longer has.
    fn edit_values(&self, ix: usize, cx: &App) -> Option<Vec<(gq_language::Property, Value)>> {
        let change = self.data.as_ref()?.folded.changes.get(ix)?;
        if change.is_edge() {
            return None;
        }
        let after = change.after.as_ref()?;
        let schema = self.session.read(cx).schema_model()?;
        let node = schema.node(&change.type_name)?;
        let values: Vec<_> = node
            .properties
            .iter()
            .filter(|p| p.editable_in_cell())
            .map(|p| {
                let value = after
                    .properties
                    .get(&p.name)
                    .cloned()
                    .unwrap_or(Value::Null);
                (p.clone(), value)
            })
            .collect();
        (!values.is_empty()).then_some(values)
    }

    /// Whether a change offers Edit, as [`Self::edit_values`] would find, without copying them:
    /// asked by every card drawn.
    fn editable(&self, ix: usize, cx: &App) -> bool {
        let Some(change) = self
            .data
            .as_ref()
            .and_then(|data| data.folded.changes.get(ix))
        else {
            return false;
        };
        if change.is_edge() || change.after.is_none() {
            return false;
        }
        self.session
            .read(cx)
            .schema_model()
            .and_then(|schema| {
                let node = schema.node(&change.type_name)?;
                Some(node.properties.iter().any(|p| p.editable_in_cell()))
            })
            .unwrap_or(false)
    }

    fn open_edit(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        // As the button is: a card fixed already shows values its fix replaced, an edit during a
        // merge could land on either side of it, and a credential refused changes writes none.
        if self.merge_may_land(cx)
            || self.cards.get(ix).is_none_or(|card| card.fix.done())
            || self.change_refusal(cx).is_some()
        {
            return;
        }
        let Some(values) = self.edit_values(ix, cx) else {
            return;
        };
        let Some(change) = self.data.as_ref().and_then(|d| d.folded.changes.get(ix)) else {
            return;
        };
        let title = format!("{} {}", change.type_name, change.id);
        let key = change.key();
        let branch = self.branch.clone();
        let view = cx.entity().downgrade();
        let Some(workspace) = workspace_for(window) else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, move |window, cx| {
                crate::edit::EditModal::new(view, key, title, branch, values, window, cx)
            })
        });
    }

    /// Writes edited fields on the reviewed branch: one statement bound to every value the
    /// review showed for them, so a node with any of them changed since is left alone whole,
    /// sent bound to the head this review holds. Never retried. An edit that cannot be sent
    /// says why, for the form to show while it stays open.
    pub(crate) fn save_edit(
        &mut self,
        key: &str,
        changes: Vec<(String, Value, Value)>,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if self.merge_may_land(cx) {
            return Err(MERGE_RUNNING.into());
        }
        if self.branch_deleted {
            return Err(BRANCH_DELETED.into());
        }
        if let Some(refused) = self.change_refusal(cx) {
            return Err(refused);
        }
        // The form was filled from the change with this key; the review may have been read
        // again since, and its cards moved.
        const GONE: &str = "The review was read again and no longer shows this change.";
        let ix = self.card_of(key).ok_or(GONE)?;
        if self.cards.get(ix).is_none_or(|card| card.fix.done()) {
            return Err("This card was fixed already: Refresh to edit it again.".into());
        }
        let change = self
            .data
            .as_ref()
            .and_then(|d| d.folded.changes.get(ix))
            .ok_or(GONE)?;
        let schema = self
            .session
            .read(cx)
            .schema_model()
            .ok_or("The schema has not loaded yet.")?;
        let lists = changes.iter().any(|(property, ..)| {
            schema
                .node(&change.type_name)
                .and_then(|node| node.property(property))
                .is_some_and(|property| property.list)
        });
        if lists && !self.shows_head() {
            return Err(LIST_NEEDS_HEAD.into());
        }
        let row = gq_language::builder::RowEdit {
            id: change.id.clone(),
            cells: changes,
        };
        let query = gq_language::builder::update_rows(&schema, &change.type_name, &[row])
            .map_err(|error| error.to_string())?;
        let request = ChangeRequest {
            query: query.text.clone(),
            name: Some(query.name.clone()),
            params: query.params_value(),
            branch: Some(self.branch.clone()),
        };
        let fix = Fix {
            key: key.to_owned(),
            request,
            description: format!("Edit {} {}", change.type_name, change.id),
            edited: true,
            after: None,
        };
        self.cards[ix].fix = FixState::Saving;
        self.queue_fix(fix, cx);
        Ok(())
    }

    /// The card of the change with `key` in the review as it is now.
    fn card_of(&self, key: &str) -> Option<usize> {
        self.data.as_ref()?.by_key.get(key).copied()
    }

    fn finish_fix(
        &mut self,
        key: &str,
        outcome: WriteOutcome<ChangeOutput>,
        edited: bool,
        cx: &mut Context<Self>,
    ) {
        self.fixes_running = self.fixes_running.saturating_sub(1);
        self.fix_out = false;
        self.fixes_in_flight.remove(key);
        let fix = match outcome {
            WriteOutcome::Committed(output) => {
                // A commit moves the branch's head even when its statement matched nothing.
                if let Some(commit) = &output.commit {
                    self.bound_head = Some(commit.graph_commit_id.clone());
                    self.preview_again = true;
                }
                match output.commit {
                    // A statement bound to values changed since matches nothing: not a fix.
                    Some(commit) if output.affected_nodes + output.affected_edges > 0 => {
                        let commit = commit.graph_commit_id;
                        if edited {
                            FixState::Edited { commit }
                        } else {
                            FixState::Reverted { commit }
                        }
                    }
                    _ => FixState::Unchanged,
                }
            }
            WriteOutcome::Refused(error) if error.status == 412 => FixState::Moved,
            WriteOutcome::Refused(error) => FixState::Refused(error.body.error),
            WriteOutcome::Unknown { .. } => FixState::Unknown,
        };
        let landed = matches!(
            fix,
            FixState::Reverted { .. } | FixState::Edited { .. } | FixState::Unchanged
        );
        let unknown = matches!(fix, FixState::Unknown);
        // Found by the change's key: a Refresh while the fix was out may have moved its card.
        if let Some(card) = self.card_of(key).and_then(|ix| self.cards.get_mut(ix)) {
            if matches!(fix, FixState::Reverted { .. }) {
                card.reviewed = true;
            }
            card.fix = fix;
        }
        if !landed {
            self.drop_fixes_after(key, unknown);
        }
        self.send_next_fix(cx);
        self.writes_settled(cx);
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    /// Drops the queued fixes that wait for the fix with `key`, which did not land, or whose
    /// outcome is `unknown`: the old relationship of a move is not restored with the new one
    /// perhaps still there.
    fn drop_fixes_after(&mut self, key: &str, unknown: bool) {
        let why = if unknown {
            "Not sent: whether the relationship that replaced it was removed is not known. \
             Check History before moving it back."
        } else {
            "Not sent: the relationship that replaced it was not removed first."
        };
        let (dropped, kept): (Vec<Fix>, Vec<Fix>) = std::mem::take(&mut self.fix_queue)
            .into_iter()
            .partition(|fix| fix.after.as_deref() == Some(key));
        self.fix_queue = kept.into();
        for fix in dropped {
            self.fixes_running = self.fixes_running.saturating_sub(1);
            self.fixes_in_flight.remove(&fix.key);
            if let Some(card) = self.card_of(&fix.key).and_then(|ix| self.cards.get_mut(ix)) {
                card.fix = FixState::Refused(why.into());
            }
        }
    }

    /// The review in a sentence: what changed, in how many commits, by whom.
    fn summary_of(&self, data: &ReviewData) -> String {
        // The commits in scope: all since the fork, or the one chosen.
        let in_scope: Vec<&ReviewCommit> = data
            .commits
            .iter()
            .filter(|c| self.scope.as_ref().is_none_or(|id| *id == c.id))
            .collect();
        let mut actors: Vec<String> = in_scope.iter().filter_map(|c| c.actor_id.clone()).collect();
        actors.sort();
        actors.dedup();
        // The commits read short of the fork are not all of them: their count and authors are
        // not said. One commit chosen is that one.
        let commits = (self.scope.is_some() || data.commits_capped.is_none())
            .then(|| in_scope.len().max(usize::from(self.scope.is_some())));
        summary_sentence(
            data.folded.entity_count(),
            data.folded.relationship_count(),
            commits,
            &actors,
        )
    }

    /// Offers Refresh when the session saw the branch's head at another commit than the one
    /// fixes and the merge bind to. Studio's own fixes move that head themselves, so a head seen
    /// while a fix or the merge is out is compared once they are back.
    fn compare_head(&mut self, cx: &mut Context<Self>) {
        if self.fixes_running > 0 || matches!(self.merge, MergeState::Merging) {
            self.head_moved_while_writing = true;
            return;
        }
        if self.newer {
            return;
        }
        let Some(bound) = self.bound_head.as_deref() else {
            return;
        };
        if self
            .session
            .read(cx)
            .head(&self.branch)
            .is_some_and(|head| head != bound)
        {
            self.mark_newer(cx);
        }
    }

    /// Someone else wrote to the branch, or its schema, after this review was read: Refresh is
    /// offered. The dry run merged the head reviewed, which the writer has moved past: its
    /// answer, shown or still to come, is not shown.
    fn mark_newer(&mut self, cx: &mut Context<Self>) {
        self.newer = true;
        self.preview = Preview::Unread;
        self._preview_task = None;
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    fn writes_settled(&mut self, cx: &mut Context<Self>) {
        self.offer_fixes_again(cx);
        if self.fixes_running == 0
            && !matches!(self.merge, MergeState::Merging)
            && std::mem::take(&mut self.head_moved_while_writing)
        {
            self.compare_head(cx);
        }
        if self.preview_again && self.fixes_running == 0 && !self.merging() {
            self.preview_moved(cx);
        }
    }

    /// Once no merge may land: a fix refused while one might may be asked for again; one that
    /// would write to a branch the merge deleted is refused when asked.
    fn offer_fixes_again(&mut self, cx: &App) {
        if self.merge_may_land(cx) {
            return;
        }
        for card in &mut self.cards {
            if matches!(&card.fix, FixState::Refused(reason) if reason == MERGE_RUNNING) {
                card.fix = FixState::Idle;
            }
        }
    }

    /// Whether Merge may run ([`Self::merge_blocked`]).
    fn mergeable(&self, cx: &App) -> bool {
        self.merge_blocked(cx).is_none()
    }

    /// Why Merge may not run now, for its tooltip (constitution VI); `None` for a review of the
    /// whole branch on screen with no fix out and no merge out, done or unsettled. A merge out
    /// or one whose lost response is not settled may still land, and a second would be sent
    /// beside it; a review being read shows a head nobody has seen; a fix still out would land
    /// on either side of the merge; a review of one commit has not shown what else the merge
    /// takes.
    fn merge_blocked(&self, cx: &App) -> Option<String> {
        Some(match &self.merge {
            MergeState::Checking | MergeState::Merging => {
                format!("Merging {} into {}…", self.branch, self.target)
            }
            MergeState::Unknown {
                landed: Check::Reading,
                ..
            } => format!(
                "Reading {}'s history for the merge sent before",
                self.target
            ),
            MergeState::Unknown {
                landed: Check::NotReached | Check::Unread,
                ..
            } => format!(
                "The merge sent before may have landed: Refresh to read {}'s history again",
                self.target
            ),
            MergeState::Merged { .. }
            | MergeState::Unknown {
                landed: Check::Shown,
                ..
            } => "This review's merge is done: Refresh to read the branch again".to_owned(),
            // Another review of these branches sent a merge whose response was lost.
            _ if self.logged_merge_may_land(cx) => format!(
                "A merge of {} into {} sent before may have landed: Refresh to read {}'s \
                 history again",
                self.branch, self.target, self.target
            ),
            _ if !matches!(self.load, LoadState::Ready) => "The review is not read yet".to_owned(),
            _ if self.fixes_running > 0 => "A fix is out: merge once it is back".to_owned(),
            _ if self.scope.is_some() => {
                format!("Review all of {}'s commits to merge it", self.branch)
            }
            _ => return None,
        })
    }

    /// Whether the merge deletes the branch: as the checkbox says, never `main`, and not when
    /// the credential may not delete it.
    fn delete_after_merge(&self, cx: &App) -> bool {
        self.delete_branch && self.branch != "main" && self.delete_refusal(cx).is_none()
    }

    /// Why the credential may not merge the branch into the target, once the access route said
    /// so (spec 002): Merge is disabled with it and its key not registered.
    fn merge_refusal(&self, cx: &App) -> Option<String> {
        self.session.read(cx).refusal(
            AccessAction::BranchMerge,
            &AccessScope::branch_into(&self.branch, &self.target),
        )
    }

    /// Why the credential may not delete the branch, from the same answer as the merge's.
    fn delete_refusal(&self, cx: &App) -> Option<String> {
        self.session.read(cx).refusal(
            AccessAction::BranchDelete,
            &AccessScope::branch_into(&self.branch, &self.target),
        )
    }

    /// Merges the branch into the target after checking the branch still sits at the head this
    /// review holds. The outcome is shown, then confirmed against the target's history.
    pub fn merge(&mut self, cx: &mut Context<Self>) {
        if !self.mergeable(cx) || self.merge_refusal(cx).is_some() {
            return;
        }
        self.merge = MergeState::Checking;
        self.show_conflicts = false;
        cx.notify();
        let session = self.session.clone();
        let branch = self.branch.clone();
        let target = self.target.clone();
        let bound = self.bound_head.clone();
        self._merge_task = Some(cx.spawn(async move |this, cx| {
            let read = session.update(cx, |session, cx| session.merge_base(&branch, &target, cx));
            let (head, before) = match read.await {
                Ok(base) => (Some(base.source_commit_id), Some(base.target_commit_id)),
                Err(error) => {
                    this.update(cx, |this, cx| {
                        this.merge = MergeState::Refused(format!(
                            "Studio could not read {branch}'s head: {error}"
                        ));
                        this.writes_settled(cx);
                        cx.notify();
                    })
                    .ok();
                    return;
                }
            };
            if head != bound {
                this.update(cx, |this, cx| {
                    this.merge = MergeState::Moved { head };
                    this.mark_newer(cx);
                    this.writes_settled(cx);
                })
                .ok();
                return;
            }
            let Ok(task) = this.update(cx, |this, cx| {
                this.merge = MergeState::Merging;
                cx.notify();
                let delete = this.delete_after_merge(cx);
                // The session's log keeps the heads the merge is sent over with its outcome.
                let sent = MergeSent {
                    source_head: bound.clone(),
                    target_head: before.clone(),
                };
                session.update(cx, |s, cx| {
                    s.merge(branch.clone(), target.clone(), sent, delete, cx)
                })
            }) else {
                return;
            };
            let outcome = task.await;
            let state = match outcome {
                WriteOutcome::Committed(output) => {
                    let BranchMergeOutput {
                        outcome,
                        commit,
                        branch_deleted,
                        branch_delete_error_details,
                        ..
                    } = output;
                    let deleted = branch_deleted.map(|deleted| {
                        if deleted {
                            Ok(())
                        } else {
                            Err(branch_delete_error_details
                                .map(|details| details.error)
                                .unwrap_or_else(|| "the server did not say why".into()))
                        }
                    });
                    // The head the merge took: the merged parent of the commit it published. A
                    // fast-forward publishes a new commit of the target too, whose merged parent
                    // is the branch head; its own id is never the branch's.
                    let took = match outcome {
                        BranchMergeOutcome::Merged | BranchMergeOutcome::FastForward => commit
                            .as_ref()
                            .and_then(|c| c.merged_parent_commit_id.clone()),
                        // An outcome this build does not know names no head to compare.
                        BranchMergeOutcome::AlreadyUpToDate | BranchMergeOutcome::Other => None,
                    };
                    let beyond_review = took.is_some() && took != bound;
                    let commit = commit.map(|c| c.graph_commit_id);
                    this.update(cx, |this, cx| {
                        // Known gone as soon as the merge says so, while what it took is
                        // checked: a fix asked for meanwhile is refused for that reason. It has
                        // no head to watch, and its review is not opened again with the window.
                        if matches!(deleted, Some(Ok(()))) {
                            this._head_watch = None;
                            this.branch_deleted = true;
                        }
                        this.merge = MergeState::Merged {
                            outcome,
                            commit: commit.clone(),
                            verified: Check::Reading,
                            beyond_review,
                            deleted: deleted.clone(),
                        };
                        cx.notify();
                    })
                    .ok();
                    let verified = merge_shown(
                        &session,
                        &branch,
                        &target,
                        commit.as_deref(),
                        bound.as_deref(),
                        before.as_deref(),
                        cx,
                    )
                    .await;
                    MergeState::Merged {
                        outcome,
                        commit,
                        verified,
                        beyond_review,
                        deleted,
                    }
                }
                WriteOutcome::Refused(error) => match error.body.merge_conflicts {
                    Some(conflicts) if !conflicts.is_empty() => MergeState::Conflicts(conflicts),
                    _ => MergeState::Refused(error.body.error),
                },
                WriteOutcome::Unknown { reason } => {
                    let sent = MergeSent {
                        source_head: bound,
                        target_head: before,
                    };
                    // Merge stays off while this read is out: a second merge would cancel it.
                    // The review's tab opened again starts from the session's log of it.
                    this.update(cx, |this, cx| {
                        this.merge = MergeState::Unknown {
                            reason: reason.clone(),
                            landed: Check::Reading,
                            sent: sent.clone(),
                        };
                        cx.notify();
                    })
                    .ok();
                    // Settle by reading history, never by merging again (constitution III).
                    let landed = merge_landed(&session, &branch, &target, &sent, cx).await;
                    MergeState::Unknown {
                        reason,
                        landed,
                        sent,
                    }
                }
            };
            this.update(cx, |this, cx| {
                this.show_conflicts = matches!(state, MergeState::Conflicts(_));
                // The session reads the branches again after a deletion it saw; after an
                // outcome nobody knows, the branch may be gone too.
                match state {
                    MergeState::Unknown {
                        reason,
                        landed,
                        sent,
                    } => {
                        this.session.update(cx, |s, cx| s.reload_branches(cx));
                        this.lost_merge_read(reason, landed, sent, cx);
                    }
                    state => this.merge = state,
                }
                this.writes_settled(cx);
                cx.emit(ItemEvent::UpdateTab);
                cx.notify();
            })
            .ok();
        }));
    }

    /// Shows the change of the conflict at `at` in the list, on its card.
    fn open_conflict(&mut self, at: usize, cx: &mut Context<Self>) {
        let Some(MergeConflictOutput {
            type_name,
            entity_id: Some(id),
            ..
        }) = self.conflicts().and_then(|conflicts| conflicts.get(at))
        else {
            return;
        };
        let Some(data) = &self.data else { return };
        if let Some(ix) = data.change_to(type_name, id) {
            self.show_conflicts = false;
            self.select(ix, cx);
        }
    }

    /// Shows the conflicts list, from its top, or the cards again.
    fn toggle_conflicts(&mut self, cx: &mut Context<Self>) {
        self.show_conflicts = !self.show_conflicts;
        if self.show_conflicts {
            self.conflict_list
                .reset(self.conflicts().map_or(0, <[_]>::len));
        }
        cx.notify();
    }

    // ---- rendering ----

    fn render_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let border = studio_ui::ds::surface::rule(cx);
        let focus = self.focus_handle.clone();
        let mergeable = self.mergeable(cx);
        let merge_refused = self.merge_refusal(cx);
        let delete_refused = self.delete_refusal(cx);
        let merge_button = studio_ui::ds::toolbar_button(
            "merge",
            format!("Merge {} into {}", self.branch, self.target),
        )
        .style(ButtonStyle::Filled)
        .start_icon(Icon::new(IconName::GitBranch))
        // A key the credential may not use is not shown, as it is not registered.
        .when(merge_refused.is_none(), |button| {
            button.key_binding(KeyBinding::for_action_in(&Merge, &focus, cx))
        })
        .disabled(!mergeable || merge_refused.is_some())
        .tooltip({
            let cli = format!(
                "omnigraph branch merge {} --into {}{}",
                self.branch,
                self.target,
                if self.delete_after_merge(cx) {
                    " --delete-branch"
                } else {
                    ""
                }
            );
            // A disabled Merge says why (constitution VI).
            let title = merge_refused
                .clone()
                .or_else(|| self.merge_blocked(cx))
                .unwrap_or_else(|| "Merge after checking the branch has not moved".to_owned());
            move |_, cx| Tooltip::with_meta(title.clone(), None, cli.clone(), cx)
        })
        .on_click(cx.listener(|this, _, _, cx| this.merge(cx)));
        let refresh = studio_ui::ds::toolbar_icon_button("refresh", IconName::RotateCw)
            .tooltip(refresh_tooltip(
                "Read the branch and its target again",
                &self.focus_handle,
            ))
            .on_click(cx.listener(|this, _, _, cx| this.reload(cx)));

        let (sentence, detail) = match (&self.load, &self.data) {
            (LoadState::Ready, Some(data)) => {
                let sentence = self.summary.clone();
                let detail = match (&data.fork, &data.head) {
                    (Some(fork), Some(head)) if data.merged_in => format!(
                        "Since {} was last merged in at {} · head {}",
                        self.target,
                        short_commit_id(fork),
                        short_commit_id(head)
                    ),
                    (Some(fork), Some(head)) => format!(
                        "Forked from {} at {} · head {}",
                        self.target,
                        short_commit_id(fork),
                        short_commit_id(head)
                    ),
                    (None, _) => format!("{} shares no history with {}", self.branch, self.target),
                    _ => String::new(),
                };
                // What a merge would do now, by the dry run read with the review; conflicts
                // show in the banner instead.
                let preview = match &self.preview {
                    Preview::Reading => " · checking the merge…".to_owned(),
                    Preview::Merges(BranchMergeOutcome::FastForward) => {
                        " · merges cleanly as a fast-forward".to_owned()
                    }
                    Preview::Merges(BranchMergeOutcome::AlreadyUpToDate) => {
                        format!(" · {} already has every change", self.target)
                    }
                    Preview::Merges(_) => " · merges cleanly".to_owned(),
                    Preview::Unread | Preview::Conflicts(_) => String::new(),
                };
                let detail = if detail.is_empty() {
                    detail
                } else {
                    format!("{detail}{preview}")
                };
                (sentence, detail)
            }
            (LoadState::Loading { folded }, _) => (
                if *folded == 0 {
                    format!("Reading what {} changed…", self.branch)
                } else {
                    format!(
                        "Reading what {} changed… {folded} changes so far",
                        self.branch
                    )
                },
                String::new(),
            ),
            (LoadState::Failed(error), _) => (
                format!("The review could not be read: {error}"),
                String::new(),
            ),
            _ => (String::new(), String::new()),
        };

        let counts = self.data.as_ref().map(|data| {
            h_flex()
                .flex_wrap()
                .gap_x(px(space::XL))
                .gap_y(px(space::XS))
                .children(data.counts.iter().map(|count| {
                    h_flex()
                        .gap(px(space::SM))
                        .child(studio_ui::kind_swatch(&count.type_name, count.is_edge, cx))
                        .child(studio_ui::ds::text::body(count.type_name.clone()))
                        .child(change_counts(count.added, count.changed, count.removed))
                }))
        });

        let mut banners: Vec<AnyElement> = Vec::new();
        if self.data.as_ref().is_some_and(|data| data.partial) {
            let text = format!(
                "Partial: showing the first {} changes, the most a review holds.",
                group_digits(REVIEW_CHANGE_CAP)
            );
            banners.push(
                studio_ui::ds::banner(Severity::Warning)
                    .child(studio_ui::ds::text::body(text))
                    .into_any_element(),
            );
        }
        if let Some(note) = self
            .data
            .as_ref()
            .and_then(|data| capped_commits_note(data, &self.branch, &self.target))
        {
            banners.push(
                studio_ui::ds::banner(Severity::Info)
                    .child(studio_ui::ds::text::body(note))
                    .into_any_element(),
            );
        }
        // Only over cards read: without them there is nothing the fixes moved past.
        let own_head = self
            .bound_head
            .as_ref()
            .filter(|_| self.data.is_some())
            .filter(|head| Some(*head) != self.reviewed_head().as_ref());
        if let (Some(head), false) = (own_head, self.newer) {
            banners.push(
                studio_ui::ds::banner(Severity::Info)
                    .child(
                        studio_ui::ds::text::body(format!(
                            "Your fixes moved {} to {}. Cards show the review as read; merging uses the new head.",
                            self.branch,
                            short_commit_id(head)
                        )),
                    )
                    .action_slot(
                        studio_ui::ds::button("refresh-own", "Refresh")
                            .tooltip(refresh_tooltip(
                                "Read the review again at the branch's head",
                                &self.focus_handle,
                            ))
                            .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
                    )
                    .into_any_element(),
            );
        }
        if self.newer && !matches!(self.merge, MergeState::Moved { .. }) {
            banners.push(
                studio_ui::ds::banner(Severity::Info)
                    .child(studio_ui::ds::text::body(format!(
                        "Newer data: {} changed after this review was read.",
                        self.branch
                    )))
                    .action_slot(
                        studio_ui::ds::button("refresh-newer", "Refresh")
                            .tooltip(refresh_tooltip(
                                "Read the review again at the branch's head",
                                &self.focus_handle,
                            ))
                            .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
                    )
                    .into_any_element(),
            );
        }
        if let Some(banner) = self.render_merge_banner(cx) {
            banners.push(banner);
        }

        // What is reviewed (everything since the fork, or one commit) and a search over the
        // cards.
        let scope_label = match &self.scope {
            None => "Since the fork".to_owned(),
            Some(id) => format!("Commit {}", short_commit_id(id)),
        };
        let view = cx.entity().downgrade();
        let scope_menu = PopoverMenu::new("review-scope")
            .trigger(
                studio_ui::ds::toolbar_button("review-scope-button", scope_label)
                    .style(ButtonStyle::Outlined)
                    .end_icon(Icon::new(IconName::ChevronDown)),
            )
            // A branch can hold thousands of commits since its fork: the menu shares the
            // review's list and draws, and labels, only the rows in view.
            .menu(move |_, cx| {
                let (commits, scope) = view.upgrade()?.read(cx).scope_menu_input();
                let view = view.clone();
                Some(cx.new(|cx| ScopeMenu::new(view, commits, scope, cx)))
            });
        let search =
            studio_ui::ds::search_field(self.search.clone(), cx).w(px(studio_ui::ds::size::FIELD));

        v_flex()
            .flex_none()
            .px(px(space::XL))
            .pt(px(space::LG))
            .pb(px(space::MD))
            .gap(px(space::MD))
            .border_b_1()
            .border_color(border)
            .child(
                h_flex()
                    .gap(px(space::MD))
                    .child(studio_ui::ds::icon::lead(IconName::GitBranch))
                    .child(studio_ui::ds::text::title(format!(
                        "{} → {}",
                        self.branch, self.target
                    )))
                    .child(div().flex_1())
                    .child(refresh)
                    .when(self.branch != "main", |row| {
                        // A branch the credential may not delete stays, unchecked, saying why.
                        let deletable = mergeable && delete_refused.is_none();
                        row.child(
                            studio_ui::ds::checkbox(
                                "delete-after-merge",
                                self.delete_branch && delete_refused.is_none(),
                                format!("Delete {}", self.branch),
                                !deletable,
                                cx,
                            )
                            .when_some(delete_refused, |checkbox, reason| {
                                checkbox.tooltip(Tooltip::text(reason))
                            })
                            .when(deletable, |checkbox| {
                                checkbox.on_click(cx.listener(|this, _, _, cx| {
                                    this.delete_branch = !this.delete_branch;
                                    cx.notify();
                                }))
                            }),
                        )
                    })
                    .child(merge_button),
            )
            .child(h_flex().gap(px(space::MD)).child(scope_menu).child(search))
            .child(studio_ui::ds::text::plain(sentence))
            .when(!detail.is_empty(), |d| {
                d.child(studio_ui::ds::text::muted(detail))
            })
            .children(counts)
            .children(banners)
            .into_any_element()
    }

    fn render_merge_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (severity, text, action): (Severity, String, Option<AnyElement>) = match &self.merge {
            MergeState::Idle => match &self.preview {
                Preview::Conflicts(conflicts) => (
                    Severity::Warning,
                    format!(
                        "{} conflict{} would stop a merge into {} now. {}",
                        conflicts.len(),
                        if conflicts.len() == 1 { "" } else { "s" },
                        self.target,
                        resolution_hint(&self.branch, &self.target)
                    ),
                    Some(
                        studio_ui::ds::button(
                            "toggle-preview-conflicts",
                            if self.show_conflicts {
                                "Show changes"
                            } else {
                                "Show conflicts"
                            },
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_conflicts(cx)))
                        .into_any_element(),
                    ),
                ),
                _ => return None,
            },
            MergeState::Checking => (
                Severity::Info,
                format!("Checking that {} has not moved…", self.branch),
                None,
            ),
            MergeState::Moved { head } => (
                Severity::Warning,
                format!(
                    "{} moved to {} after this review was read. Refresh and review the new changes before merging.",
                    self.branch,
                    head.as_deref()
                        .map(short_commit_id)
                        .unwrap_or_else(|| "nothing".into())
                ),
                Some(
                    studio_ui::ds::button("refresh-moved", "Refresh")
                        .tooltip(refresh_tooltip(
                            "Read the review again at the branch's head",
                            &self.focus_handle,
                        ))
                        .on_click(cx.listener(|this, _, _, cx| this.reload(cx)))
                        .into_any_element(),
                ),
            ),
            MergeState::Merging => (
                Severity::Info,
                format!("Merging {} into {}…", self.branch, self.target),
                None,
            ),
            MergeState::Merged {
                outcome,
                commit,
                verified,
                beyond_review,
                deleted,
            } => {
                let what = match outcome {
                    BranchMergeOutcome::FastForward => {
                        format!("Fast-forwarded {} to {}.", self.target, self.branch)
                    }
                    BranchMergeOutcome::Merged => format!(
                        "Merged {} into {} as commit {}.",
                        self.branch,
                        self.target,
                        commit.as_deref().map(short_commit_id).unwrap_or_default()
                    ),
                    BranchMergeOutcome::AlreadyUpToDate => {
                        format!(
                            "{} is already up to date with {}.",
                            self.target, self.branch
                        )
                    }
                    BranchMergeOutcome::Other => {
                        format!("Merged {} into {}.", self.branch, self.target)
                    }
                };
                let check = merged_check_words(*verified, &self.target);
                let beyond = if *beyond_review {
                    format!(
                        " The merge took a commit of {} made after the review was read; open \
                         History to see what else landed.",
                        self.branch
                    )
                } else {
                    String::new()
                };
                let deletion = match deleted {
                    None => String::new(),
                    Some(Ok(())) => format!(" Deleted {}.", self.branch),
                    Some(Err(why)) => format!(" {} was not deleted: {why}.", self.branch),
                };
                let beyond = format!("{beyond}{deletion}");
                (
                    if matches!(
                        verified,
                        Check::NotShown | Check::NotReached | Check::Unread
                    ) || *beyond_review
                        || matches!(deleted, Some(Err(_)))
                    {
                        Severity::Warning
                    } else {
                        Severity::Success
                    },
                    format!("{what}{check}{beyond}"),
                    None,
                )
            }
            MergeState::Conflicts(conflicts) => (
                Severity::Error,
                format!(
                    "{} conflict{} stopped the merge, and nothing was merged. {}",
                    conflicts.len(),
                    if conflicts.len() == 1 { "" } else { "s" },
                    resolution_hint(&self.branch, &self.target)
                ),
                Some(
                    studio_ui::ds::button(
                        "toggle-conflicts",
                        if self.show_conflicts {
                            "Show changes"
                        } else {
                            "Show conflicts"
                        },
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_conflicts(cx)))
                    .into_any_element(),
                ),
            ),
            MergeState::Refused(message) => (
                Severity::Error,
                format!("The merge was refused: {message}. Nothing was merged."),
                None,
            ),
            MergeState::Unknown { reason, landed, .. } => (
                Severity::Warning,
                unknown_merge_words(reason, *landed, &self.target),
                None,
            ),
        };
        let mut banner = studio_ui::ds::banner(severity)
            .wrap_content(true)
            .child(studio_ui::ds::text::body(text));
        if let Some(action) = action {
            banner = banner.action_slot(action);
        }
        Some(banner.into_any_element())
    }

    fn render_conflicts(&self, cx: &mut Context<Self>) -> AnyElement {
        let count = self.conflicts().map_or(0, <[_]>::len);
        // A merge's own conflicts, or a dry run's read again, replace the list shown.
        if self.conflict_list.item_count() != count {
            self.conflict_list.reset(count);
        }
        v_flex()
            .flex_1()
            .min_h_0()
            .pt(px(space::XL))
            .gap(px(space::MD))
            .child(div().px(px(space::XL)).child(studio_ui::ds::text::muted(
                if matches!(self.merge, MergeState::Conflicts(_)) {
                    format!("Conflicts merging {} into {}", self.branch, self.target)
                } else {
                    format!(
                        "Conflicts a merge of {} into {} would stop on",
                        self.branch, self.target
                    )
                },
            )))
            .child(
                list(
                    self.conflict_list.clone(),
                    cx.processor(|this, ix, _, cx| this.render_conflict(ix, cx)),
                )
                .flex_1()
                .min_h_0(),
            )
            .into_any_element()
    }

    /// One conflict of the list, made only while it is on screen.
    fn render_conflict(&mut self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(conflict) = self.conflicts().and_then(|conflicts| conflicts.get(ix)) else {
            return div().into_any_element();
        };
        // What the branch did to the conflict's entity.
        let here = conflict.entity_id.as_deref().and_then(|id| {
            let data = self.data.as_ref()?;
            data.folded.changes[data.change_to(&conflict.type_name, id)?].net()
        });
        let words = conflict_wording(conflict.kind, here, &self.target);
        let entity = conflict.entity_id.clone().unwrap_or_else(|| "—".into());
        div()
            .px(px(space::XL))
            .pb(px(space::MD))
            .child(
                h_flex()
                    .id(("conflict", ix))
                    .w_full()
                    .gap(px(space::LG))
                    .px(px(space::LG))
                    .py(px(space::MD))
                    .rounded(px(radii(cx).md))
                    .border_1()
                    .border_color(studio_ui::ds::surface::rule(cx))
                    .bg(studio_ui::ds::surface::document(cx))
                    .child(Icon::new(IconName::GitMergeConflict).color(Color::Conflict))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(studio_ui::ds::text::plain(words).color(Color::Conflict))
                            .child(
                                h_flex()
                                    .gap(px(space::MD))
                                    .child(type_swatch(&conflict.type_name, cx))
                                    .child(studio_ui::ds::text::muted(conflict.type_name.clone()))
                                    .child(studio_ui::ds::text::body(entity).buffer_font(cx)),
                            )
                            .child(studio_ui::ds::text::meta(conflict.message.clone())),
                    )
                    .when(here.is_some(), |d| {
                        d.cursor_pointer()
                            .child(studio_ui::ds::text::meta("Show change").color(Color::Accent))
                            .on_click(cx.listener(move |this, _, _, cx| this.open_conflict(ix, cx)))
                    }),
            )
            .into_any_element()
    }

    fn render_row(
        &mut self,
        row: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(data) = &self.data else {
            return div().into_any_element();
        };
        match self.rows.get(row).copied() {
            Some(Row::Header(ix)) => div()
                .px(px(space::XL))
                .child(change_cards::run_header(
                    &self.headers[ix],
                    |section| match section {
                        GroupSection::Existing => format!("Changes to what {} has", self.target),
                        GroupSection::Added => format!("Added on {}", self.branch),
                    },
                    cx,
                ))
                .into_any_element(),
            Some(Row::Entity(group)) => {
                // The schema the groups were made with, which the cards' rows agree with.
                let card = self.entity_card(data, group, data.schema.as_deref(), cx);
                div()
                    .px(px(space::XL))
                    .py(px(space::XS))
                    .child(card)
                    .into_any_element()
            }
            None => div().into_any_element(),
        }
    }

    /// One entity's card: its own change, with Mark reviewed, Edit and Revert, then each
    /// relationship change reviewed on it, each with its own Revert, then the relationships
    /// reviewed on another card that it is an end of.
    fn entity_card(
        &self,
        data: &ReviewData,
        ix: usize,
        schema: Option<&SchemaModel>,
        cx: &mut Context<Self>,
    ) -> ChangeCard {
        let group = &data.groups.groups[ix];
        let focus = self.focus_handle.clone();
        let selected_group = self.selected.and_then(|s| data.groups.home.get(s).copied());
        let unreviewed = self.group_unreviewed(group);
        let total = usize::from(group.change.is_some()) + group.relationships.len();
        let reviewed = unreviewed == 0;

        let mut card = change_cards::entity_card(
            &data.groups,
            &data.folded,
            ix,
            schema,
            &self.names,
            &data.card_rows,
            ("change", ix),
        )
        .selected(selected_group == Some(ix))
        .hollow(reviewed);
        if let Some((text, color)) = group
            .change
            .and_then(|own| fix_status(&self.cards[own].fix))
        {
            card = card.status(text, color);
        } else if !reviewed && unreviewed < total {
            card = card.status(
                format!("{unreviewed} of {total} not reviewed"),
                Color::Muted,
            );
        }

        let review_button = studio_ui::ds::row_button(
            ("reviewed", ix),
            if reviewed {
                "Reviewed"
            } else {
                "Mark reviewed"
            },
        )
        .style(ButtonStyle::Subtle)
        .toggle_state(reviewed)
        .when(reviewed, |b| b.start_icon(Icon::new(IconName::Check)))
        .tooltip({
            let focus = focus.clone();
            move |_, cx| {
                Tooltip::for_action_in(
                    "Mark this card reviewed and go to the next",
                    &AcceptAndNext,
                    &focus,
                    cx,
                )
            }
        })
        .on_click(cx.listener(move |this, _, _, cx| this.toggle_reviewed(ix, cx)));
        card = card.action(review_button);

        // A credential the access route refused changes on the branch writes nothing here: Edit
        // and every Revert are disabled with that reason (spec 002).
        let refused = self.change_refusal(cx);
        if let Some(own) = group.change {
            let state = &self.cards[own];
            let done = state.fix.done() || self.merge_may_land(cx);
            if self.editable(own, cx) {
                let branch = self.branch.clone();
                let focus = focus.clone();
                let refused = refused.clone();
                card = card.action(
                    studio_ui::ds::row_button(("edit", ix), "Edit")
                        .style(ButtonStyle::Subtle)
                        .start_icon(Icon::new(IconName::Pencil))
                        .disabled(done || refused.is_some())
                        .tooltip(move |_, cx| match &refused {
                            Some(reason) => Tooltip::simple(reason.clone(), cx),
                            None => Tooltip::for_action_in(
                                format!("Change what {branch} holds now"),
                                &EditChange,
                                &focus,
                                cx,
                            ),
                        })
                        .on_click(
                            cx.listener(move |this, _, window, cx| this.open_edit(own, window, cx)),
                        ),
                );
            }
            let blocked = refused.clone().or_else(|| state.revert_blocked.clone());
            let branch = self.branch.clone();
            card = card.action(
                studio_ui::ds::row_button(("revert", ix), "Revert")
                    .style(ButtonStyle::Subtle)
                    .start_icon(Icon::new(IconName::Undo))
                    .disabled(blocked.is_some() || done)
                    .tooltip(move |_, cx| match &blocked {
                        Some(reason) => Tooltip::simple(reason.clone(), cx),
                        None => Tooltip::with_meta(
                            "Restore the value before this branch changed it",
                            Some(&RevertAndNext),
                            format!("Writes to {branch}, bound to the reviewed head"),
                            cx,
                        ),
                    })
                    .on_click(cx.listener(move |this, _, _, cx| this.revert(own, cx))),
            );
        }

        // A card of many relationships shows its first few and more as they are asked for, and
        // grows by itself through a relationship selected below them (from a mirror, or a
        // change of a fix). Only the lines it shows are made.
        let card_key = (group.type_name.clone(), group.id.clone());
        let total = data.groups.line_count(ix);
        // Looked for on the card the selection is reviewed on only, not through every card's
        // lines at every frame.
        let selected_at = self
            .selected
            .filter(|_| selected_group == Some(ix))
            .and_then(|s| data.groups.line_of(ix, s));
        let shown = self.card_lines.shown(&card_key, total, selected_at);
        let left_out = total - shown;
        let card_lines = data
            .groups
            .lines(&data.folded, ix, schema, &self.names, shown);
        let counts = (card_lines.added, card_lines.changed, card_lines.removed);
        let lines = card_lines
            .entries
            .into_iter()
            .map(|entry| {
                let change = entry.change;
                let mirror = entry.net.is_none();
                let moved_from = entry.replaces.as_ref().map(|(removed, _)| *removed);
                // A mirror's other end is the card it is reviewed on.
                let mut line = change_cards::relationship_line(
                    entry,
                    &data.folded,
                    schema,
                    &data.card_rows,
                    "reviewed there",
                )
                .on_click(cx.listener(move |this, _, _, cx| this.select(change, cx)));
                if mirror {
                    return line;
                }
                let state = &self.cards[change];
                let was = moved_from.map(|removed| &self.cards[removed]);
                let hover = SharedString::from(format!("relationship-{change}"));
                let blocked = refused
                    .clone()
                    .or_else(|| state.revert_blocked.clone())
                    .or_else(|| was.and_then(|was| was.revert_blocked.clone()));
                let done = state.fix.done()
                    || was.is_some_and(|was| was.fix.done())
                    || self.merge_may_land(cx);
                let reviewed = state.reviewed && was.is_none_or(|was| was.reviewed);
                let fix = match was {
                    Some(was) if state.fix == FixState::Idle => was.fix.clone(),
                    _ => state.fix.clone(),
                };
                let branch = self.branch.clone();
                let revert =
                    studio_ui::ds::row_icon_button(("revert-relationship", change), IconName::Undo)
                        .style(ButtonStyle::Subtle)
                        .visible_on_hover(hover.clone())
                        .disabled(blocked.is_some() || done)
                        .tooltip(move |_, cx| match &blocked {
                            Some(reason) => Tooltip::simple(reason.clone(), cx),
                            None => Tooltip::with_meta(
                                if moved_from.is_some() {
                                    "Move this relationship back"
                                } else {
                                    "Revert this relationship change"
                                },
                                Some(&RevertAndNext),
                                format!("Writes to {branch}, bound to the reviewed head"),
                                cx,
                            ),
                        })
                        .on_click(cx.listener(move |this, _, _, cx| this.revert_line(change, cx)));
                // The selected change is what ⌥⌘Z reverts: a card without its own change selects
                // its first relationship, which shows it.
                line = line
                    .selected(
                        self.selected
                            .is_some_and(|s| s == change || Some(s) == moved_from),
                    )
                    .hollow(reviewed)
                    .hover_group(hover)
                    .action(revert);
                if let Some((text, color)) = fix_status(&fix) {
                    line = line.status(text, color);
                }
                line
            })
            .collect();
        card = card.relationships(counts, lines);
        if left_out > 0 {
            card = card.relationships_footer(
                studio_ui::ds::button(
                    ("more-relationships", ix),
                    change_cards::more_lines_label(left_out),
                )
                .style(ButtonStyle::Subtle)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.card_lines.show_more(card_key.clone(), shown);
                    cx.notify();
                })),
            );
        }
        card.on_click(cx.listener(move |this, _, _, cx| {
            let primary = this
                .data
                .as_ref()
                .and_then(|data| data.groups.groups[ix].primary());
            if let Some(change) = primary {
                this.select(change, cx);
            }
        }))
    }
}

/// What the merge banner says the target's history showed of a merge that committed.
fn merged_check_words(verified: Check, target: &str) -> String {
    match verified {
        Check::Reading => format!(" Checking {target}'s history…"),
        Check::Shown => format!(" Confirmed in {target}'s history."),
        Check::NotShown => format!(" Not found in {target}'s history yet; open History to check."),
        Check::NotReached => format!(
            " Not confirmed: the newest {} commits of {target}'s history do not show it, and \
             Studio read no further; open History to check.",
            group_digits(FORK_BOUND)
        ),
        Check::Unread => {
            format!(" {target}'s history could not be read to confirm it; open History to check.")
        }
    }
}

/// What the merge banner says of a merge whose response was lost, by what the target's history
/// showed of it (constitution III: unknown until history says otherwise, and never sent again).
/// History that does not show it yet is no proof it will not land: the merge may still be
/// running on the server.
fn unknown_merge_words(reason: &str, landed: Check, target: &str) -> String {
    match landed {
        Check::Reading => format!(
            "The merge outcome is unknown ({reason}). Studio did not retry; reading {target}'s history…"
        ),
        Check::Shown => format!(
            "The merge response was lost ({reason}), but {target}'s history shows the merge landed."
        ),
        Check::NotShown => format!(
            "The merge response was lost ({reason}) and {target}'s history does not show it yet, but the merge may still land. Studio did not retry; open History to check before merging again."
        ),
        Check::NotReached => format!(
            "The merge outcome is unknown ({reason}): the newest {} commits of {target}'s history do not show it, and Studio read no further. Studio did not retry, and Merge stays off until {target}'s history settles it: open History to check, or Refresh to read it again.",
            group_digits(FORK_BOUND)
        ),
        Check::Unread => format!(
            "The merge outcome is unknown ({reason}), and {target}'s history could not be read to tell. Studio did not retry, and Merge stays off until {target}'s history settles it: open History to check, or Refresh to read it again."
        ),
    }
}

/// What a review of `branch` into `target` says when the fork lies past the history it read:
/// which history stopped it, that it read no further, and what the scope menu's One commit
/// lists. Counts are of commits at or below the reviewed head, never of the commits the branch
/// made after it, which the pages read too.
fn capped_commits_note(data: &ReviewData, branch: &str, target: &str) -> Option<String> {
    let commits = |n: usize| {
        format!(
            "{} commit{}",
            group_digits(n),
            if n == 1 { "" } else { "s" }
        )
    };
    Some(match data.commits_capped.as_ref()? {
        Capped::Own { read: 0 } => format!(
            "{branch} has more than {} commits since this review was read: Studio read no \
             further. Refresh to review its head.",
            group_digits(FORK_BOUND)
        ),
        Capped::Own { read } => format!(
            "More than {} since the fork: Studio read no further, and One commit lists only \
             those it read.",
            commits(*read)
        ),
        Capped::Fork { made, target: true } => format!(
            "The fork is past the newest {} of {target}'s history: Studio read no further, and \
             One commit lists the {} made on {branch}.",
            commits(FORK_BOUND),
            commits(*made)
        ),
        Capped::Fork {
            made,
            target: false,
        } => format!(
            "The fork is more than {} down {branch}'s history: Studio read no further, and One \
             commit lists the {} made on {branch}.",
            commits(FORK_BOUND),
            commits(*made)
        ),
    })
}

/// What a fix on a change says on its card or line, once one was asked for.
fn fix_status(fix: &FixState) -> Option<(String, Color)> {
    Some(match fix {
        FixState::Idle => return None,
        FixState::Running => ("Reverting…".to_owned(), Color::Muted),
        FixState::Saving => ("Saving…".to_owned(), Color::Muted),
        FixState::Reverted { commit } => (
            format!("Reverted in {}", short_commit_id(commit)),
            Color::Success,
        ),
        FixState::Edited { commit } => (
            format!("Edited in {}", short_commit_id(commit)),
            Color::Success,
        ),
        FixState::Unchanged => (
            "Not written: the value changed since this review. Refresh.".to_owned(),
            Color::Warning,
        ),
        FixState::Moved => (
            "Not written: the branch moved. Refresh, then try again.".to_owned(),
            Color::Warning,
        ),
        FixState::Refused(message) => (format!("Refused: {message}"), Color::Error),
        FixState::Unknown => (
            "Outcome unknown. Check History before trying again.".to_owned(),
            Color::Warning,
        ),
    })
}

/// What a search finds a change by, lowercase: its id, its type, a relationship's two ends and
/// each value before and after, one per line, so a search never matches across two of them.
fn search_text_of(change: &FoldedChange) -> String {
    let mut text = format!("{}\n{}", change.id, change.type_name);
    if let Some((from, to)) = change.endpoints() {
        text.push_str(&format!("\n{from}\n{to}"));
    }
    for value in [&change.before, &change.after]
        .into_iter()
        .flatten()
        .flat_map(|image| image.properties.values())
    {
        text.push('\n');
        match value {
            Value::String(value) => text.push_str(value),
            other => text.push_str(&other.to_string()),
        }
    }
    text.to_lowercase()
}

/// The list's rows: each card the search leaves (`matched`, each change's match; every card
/// without one), under a header for each run of one type, the first of a section carrying its
/// title. A card matches when its own change or a relationship reviewed on it does. The headers
/// count what the run shows.
fn build_rows(data: &ReviewData, matched: Option<&[bool]>) -> (Vec<Row>, Vec<GroupRun>) {
    let matches =
        |change: usize| matched.is_none_or(|matched| matched.get(change).copied().unwrap_or(false));
    change_cards::rows(&data.folded, &data.groups, |group| {
        group.changes().any(matches)
    })
}

/// Reads what the branch changed since its merge base with the target: the base and the head
/// from `branches/merge-base`, then the branch's change feed after the base, folded up to the
/// head and the change cap. A review of one commit, and a base the feed refuses as a start
/// (off the branch's first-parent history, after the target was merged in), read both
/// histories instead. A merge base and commits since the fork already read for heads that still
/// stand (`held`) are taken as read.
async fn read_review(
    session: Entity<GraphSession>,
    branch: String,
    target: String,
    scope: Option<String>,
    held: Option<ForkRead>,
    this: WeakEntity<ReviewView>,
    cx: &mut AsyncApp,
) -> Result<ReviewData, String> {
    let read = match held {
        Some(read) => read,
        None => {
            let base = session
                .update(cx, |session, cx| session.merge_base(&branch, &target, cx))
                .await
                .map_err(|e| e.to_string())?;
            ForkRead {
                base: base.merge_base.graph_commit_id,
                head: base.source_commit_id,
                target_head: base.target_commit_id,
                since: [None, None],
            }
        }
    };
    if scope.is_none() {
        let (fork, head) = (read.base.clone(), read.head.clone());
        if fork == head {
            return Ok(ReviewData {
                fork: Some(fork),
                head: Some(head),
                commits: Vec::new().into(),
                fork_read: Some(read),
                folded: Arc::new(ChangeFold::new().finish()),
                groups: EntityGroups::default(),
                search: SearchIndex::default(),
                counts: Vec::new(),
                revert_blocked: Vec::new(),
                by_entity: Vec::new(),
                by_key: HashMap::new(),
                card_rows: CardRows::default(),
                marks: Vec::new(),
                schema: session.read_with(cx, |session, _| session.schema_model()),
                partial: false,
                moved_while_reading: false,
                merged_in: false,
                commits_capped: None,
            });
        }
        let start = format!("after:{fork}");
        return match fold_feed(&session, &branch, start, &head, &this, cx).await {
            Ok(fed) => {
                let mut data = fed
                    .into_review(&session, Some(fork), Some(head), false, cx)
                    .await;
                data.fork_read = Some(read);
                Ok(data)
            }
            Err(FeedError::Refused) => {
                read_review_since_merge(session, branch, target, read, this, cx).await
            }
            Err(FeedError::Failed(error)) => Err(error),
        };
    }
    read_commit_review(session, branch, target, scope, read, this, cx).await
}

/// What folding a branch's change feed gave.
struct Fed {
    fold: ChangeFold,
    /// The commits folded, oldest first.
    commits: Vec<ReviewCommit>,
    partial: bool,
    moved_while_reading: bool,
}

impl Fed {
    /// The review of what was folded. A review of one commit (`one_commit`), like one the
    /// change cap cut short, does not show what the branch holds now.
    async fn into_review(
        self,
        session: &Entity<GraphSession>,
        fork: Option<String>,
        head: Option<String>,
        one_commit: bool,
        cx: &mut AsyncApp,
    ) -> ReviewData {
        let fold = self.fold;
        let shows_head = !one_commit && !self.partial;
        let schema = session.read_with(cx, |session, _| session.schema_model());
        let made_with = schema.clone();
        let (folded, groups, search, revert_blocked, by_entity, by_key, marks) = cx
            .background_executor()
            .spawn(async move {
                let folded = Arc::new(fold.finish());
                let groups = EntityGroups::new(&folded, schema.as_deref());
                let search = SearchIndex::new(folded.clone());
                let marks = folded.changes.iter().map(mark_of).collect();
                let revert_blocked = revert_checks(&folded, schema.as_deref(), shows_head);
                let by_entity = entity_order(&folded.changes);
                let by_key: HashMap<String, usize> = folded
                    .changes
                    .iter()
                    .enumerate()
                    .map(|(ix, change)| (change.key(), ix))
                    .collect();
                (
                    folded,
                    groups,
                    search,
                    revert_blocked,
                    by_entity,
                    by_key,
                    marks,
                )
            })
            .await;
        let counts = folded.counts();
        let mut commits = self.commits;
        commits.reverse();
        ReviewData {
            fork,
            head,
            commits: commits.into(),
            fork_read: None,
            folded,
            groups,
            search,
            counts,
            revert_blocked,
            by_entity,
            by_key,
            card_rows: CardRows::default(),
            marks,
            schema: made_with,
            partial: self.partial,
            moved_while_reading: self.moved_while_reading,
            merged_in: false,
            commits_capped: None,
        }
    }
}

enum FeedError {
    /// The feed refused the start commit.
    Refused,
    Failed(String),
}

/// Folds `branch`'s change feed from `start` (`after:<fork>`) up to `head`, the change cap, or
/// a block after `head`.
async fn fold_feed(
    session: &Entity<GraphSession>,
    branch: &str,
    start: String,
    head: &str,
    this: &WeakEntity<ReviewView>,
    cx: &mut AsyncApp,
) -> Result<Fed, FeedError> {
    let mut fold = ChangeFold::new();
    let mut commits = Vec::new();
    let mut partial = false;
    let mut moved_while_reading = false;
    let mut reached_head = false;
    let mut start = Some(start);
    let mut token: Option<String> = None;
    loop {
        let limit = FEED_PAGE.min(REVIEW_CHANGE_CAP - fold.raw_changes()).max(1) as u32;
        let first = commits.is_empty() && token.is_none();
        let page = session
            .update(cx, |session, cx| {
                session.changes(branch, start.clone(), token.clone(), limit, cx)
            })
            .await
            .map_err(|error| match error {
                ReadError::Refused(refusal) if first && refusal.status == 400 => FeedError::Refused,
                error => FeedError::Failed(error.to_string()),
            })?;
        let mut blocks = Vec::new();
        let mut last_commit = None;
        for block in page.blocks {
            let id = &block.cause.graph_commit_id;
            if reached_head && commits.last().map(|c: &ReviewCommit| &c.id) != Some(id) {
                // Past the head the review read: the branch moved while reading.
                moved_while_reading = true;
                break;
            }
            if commits.last().map(|c: &ReviewCommit| &c.id) != Some(id) {
                commits.push(ReviewCommit {
                    id: id.clone(),
                    actor_id: block.cause.actor_id.clone(),
                    created_at: block.cause.authored_at,
                });
            }
            last_commit = Some(id.clone());
            reached_head |= id == head;
            blocks.push(block);
        }
        let (folded, complete) = fold_blocks(fold, blocks, this, cx).await;
        fold = folded;
        let raw = fold.raw_changes();
        let more =
            page.next_page_token.is_some() || (!reached_head && page.caught_up == Some(false));
        if !complete || (raw >= REVIEW_CHANGE_CAP && more) {
            partial = true;
            break;
        }
        if moved_while_reading {
            break;
        }
        if let Some(next) = page.next_page_token {
            token = Some(next);
            start = None;
            continue;
        }
        token = None;
        if reached_head || page.caught_up != Some(false) {
            break;
        }
        match last_commit {
            Some(id) => start = Some(format!("after:{id}")),
            None => break,
        }
    }
    Ok(Fed {
        fold,
        commits,
        partial,
        moved_while_reading,
    })
}

/// Folds `blocks` into `fold` off the main thread (constitution IX), up to the change cap, and
/// shows how many changes are folded: the fold, and whether every change fit.
async fn fold_blocks(
    mut fold: ChangeFold,
    blocks: Vec<ChangeBlockOutput>,
    this: &WeakEntity<ReviewView>,
    cx: &mut AsyncApp,
) -> (ChangeFold, bool) {
    let (fold, complete) = cx
        .background_executor()
        .spawn(async move {
            let mut complete = true;
            for block in blocks {
                complete &= fold.add_block_capped(block, REVIEW_CHANGE_CAP);
            }
            (fold, complete)
        })
        .await;
    let folded = fold.raw_changes();
    this.update(cx, |this, cx| {
        this.load = LoadState::Loading { folded };
        cx.notify();
    })
    .ok();
    (fold, complete)
}

/// Folds a paged list of changes up to the change cap: `read` reads the page a token names
/// (the first for none), at most `limit` changes, as one block and the next page's token. The
/// fold, and whether the cap left changes out.
async fn fold_pages(
    this: &WeakEntity<ReviewView>,
    cx: &mut AsyncApp,
    mut read: impl AsyncFnMut(
        Option<String>,
        u32,
        &mut AsyncApp,
    ) -> Result<(ChangeBlockOutput, Option<String>), String>,
) -> Result<(ChangeFold, bool), String> {
    let mut fold = ChangeFold::new();
    let mut token = None;
    loop {
        let limit = FEED_PAGE.min(REVIEW_CHANGE_CAP - fold.raw_changes()).max(1) as u32;
        let (block, next) = read(token.take(), limit, cx).await?;
        let (folded, complete) = fold_blocks(fold, vec![block], this, cx).await;
        fold = folded;
        match next {
            Some(next) if complete && fold.raw_changes() < REVIEW_CHANGE_CAP => token = Some(next),
            Some(_) => return Ok((fold, true)),
            None => return Ok((fold, false)),
        }
    }
}

/// A branch's history read a page of [`FORK_PAGE`] at a time, newest first, for at most
/// [`FORK_PAGES`] pages. Later pages continue the listing the first resolved, so they answer
/// for that head while the branch moves on.
struct Pages<'a> {
    branch: &'a str,
    token: Option<String>,
    read: usize,
    /// The listing has no page after the last read.
    done: bool,
}

impl<'a> Pages<'a> {
    fn new(branch: &'a str) -> Self {
        Self {
            branch,
            token: None,
            read: 0,
            done: false,
        }
    }

    /// Whether another page may be read: the listing goes on, and the bound is not reached.
    fn more(&self) -> bool {
        !self.done && self.read < FORK_PAGES
    }

    /// Asks for the next page: the read starts now, beside any other, and counts once its
    /// answer is taken ([`Self::take`]). One dropped unread stops, and counts for nothing.
    fn ask(
        &self,
        session: &Entity<GraphSession>,
        cx: &mut AsyncApp,
    ) -> Task<Result<CommitListOutput, ReadError>> {
        let (branch, token) = (self.branch, self.token.clone());
        session.update(cx, |session, cx| {
            session.commit_page(branch, FORK_PAGE, token, cx)
        })
    }

    /// Takes the page asked for: the listing goes on from its token.
    fn take(&mut self, page: CommitListOutput) -> Vec<CommitOutput> {
        self.read += 1;
        // A page with no commits ends the listing: a token after nothing would only ask for
        // nothing again.
        self.token = page.next_page_token.filter(|_| !page.commits.is_empty());
        self.done = self.token.is_none();
        page.commits
    }

    async fn next(
        &mut self,
        session: &Entity<GraphSession>,
        cx: &mut AsyncApp,
    ) -> Result<Vec<CommitOutput>, ReadError> {
        let page = self.ask(session, cx).await?;
        Ok(self.take(page))
    }
}

/// A branch's commits since it forked from its target, as far as the histories read reach.
#[derive(Debug, Clone)]
struct Since {
    fork: Option<String>,
    head: String,
    /// Newest first.
    commits: Vec<ReviewCommit>,
    /// The target was merged into the branch since it forked: the merge base is off the
    /// branch's own chain.
    merged_in: bool,
    /// The fork lies past the commits read: `commits` are the branch's own read, `fork` the
    /// merge base, and this says which history stopped the read.
    capped: Option<Capped>,
}

/// Which history a review stopped reading before it reached the fork, for the note that says
/// so ([`capped_commits_note`]).
#[derive(Debug, Clone, PartialEq)]
enum Capped {
    /// The branch's own pages ran out while its own commits went on: `read` of them were read at
    /// or below the reviewed head (none when every page read was of commits made after it).
    Own { read: usize },
    /// Every commit the branch made since the reviewed head was read, `made` of them, and the
    /// fork lies past what was read of a history: the target's (`target`), or the branch's own
    /// below them.
    Fork { made: usize, target: bool },
}

/// Whether `commit` is one the branch published: the server records each commit with the branch
/// that wrote it (`None` for main), so a branch's chain holds its own commits, then those of the
/// branch it forked from, which it shares with that branch.
fn made_on(commit: &CommitOutput, branch: &str) -> bool {
    commit.graph_branch.as_deref().unwrap_or("main") == branch
}

/// The branch's commits since it forked from `target`, newest first, with the fork, read a page
/// at a time ([`Pages`], [`ForkSearch`]).
///
/// The branch's first-parent chain is read from the reviewed `head` down to the merge base the
/// server names (`base`). A branch the target was merged into (`merged_in`, or a commit of its
/// chain naming `base` as its merged parent) has its merge base off its chain: its fork is the
/// newest commit of its chain that the target's history holds, so the target's history is read
/// too, a page with each of the branch's. So is it for a chain that ends without reaching
/// `base`. Once the target's page is known to be needed, whatever the branch's next page shows,
/// the two are asked for together, so their reads overlap; they are taken in the order a read
/// of one after the other takes them, so the answer is the same, and a target page the branch's
/// page made needless (it reached the fork) is dropped unread. A fork not reached within
/// [`FORK_PAGES`] pages of each history leaves `capped`, with `base` for the fork: the review
/// says so rather than reading on, and its commits are the branch's own read, down to the first
/// commit another branch published (the history it shares with the branch it forked from,
/// which the pages go on into).
async fn commits_since_fork(
    session: &Entity<GraphSession>,
    branch: &str,
    target: &str,
    base: &str,
    head: &str,
    mut merged_in: bool,
    cx: &mut AsyncApp,
) -> Result<Since, String> {
    let mut search = ForkSearch::default();
    search.hold(base);
    let (mut own, mut theirs) = (Pages::new(branch), Pages::new(target));
    // The listing starts at the branch's head now: commits made after the reviewed head are
    // not the review's.
    let mut above_head = true;
    // The chain's commits read, at or below the reviewed head, and how many of them the branch
    // made before the first commit another branch published: known once that commit is read.
    let mut chained = 0;
    let mut made = None;
    while search.fork().is_none() {
        // The target's history decides the fork only for a merge base off the branch's chain,
        // or a chain that ended short of it. Known before this round, its page is asked for
        // beside the branch's.
        let own_read = own.more().then(|| own.ask(session, cx));
        let mut target_read =
            ((merged_in || own.done) && theirs.more()).then(|| theirs.ask(session, cx));
        if own_read.is_none() && target_read.is_none() {
            break;
        }
        if let Some(read) = own_read {
            let page = own.take(read.await.map_err(|e| e.to_string())?);
            let mut chain = Vec::with_capacity(page.len());
            for commit in page {
                if above_head {
                    if commit.graph_commit_id != head {
                        continue;
                    }
                    above_head = false;
                }
                merged_in |= commit.merged_parent_commit_id.as_deref() == Some(base);
                if made.is_none() && !made_on(&commit, branch) {
                    made = Some(chained);
                }
                chained += 1;
                chain.push(commit);
            }
            search.branch_page(chain);
        }
        // Needed only now (the branch's page named the merge base as a merged parent, or ended
        // its chain), the target's page is asked for after it. Past the fork, a page asked for
        // beside the branch's is dropped unread.
        if search.fork().is_none() && (merged_in || own.done) {
            if target_read.is_none() && theirs.more() {
                target_read = Some(theirs.ask(session, cx));
            }
            if let Some(read) = target_read {
                let page = theirs.take(read.await.map_err(|e| e.to_string())?);
                search.target_page(&page);
            }
        }
    }
    // Both histories read to their ends and sharing nothing is an answer; anything less is
    // the bound.
    let capped = (search.fork().is_none() && !(own.done && theirs.done)).then(|| match made {
        // The branch's own commits went on past its pages.
        None if !own.done => Capped::Own { read: chained },
        made => Capped::Fork {
            made: made.unwrap_or(chained),
            target: theirs.read > 0 && !theirs.done,
        },
    });
    let fork = match search.fork() {
        Some(fork) => Some(fork.graph_commit_id.clone()),
        None if capped.is_some() => Some(base.to_owned()),
        None => None,
    };
    let since = search.since();
    // Short of the fork, the pages go on into history shared with another branch: those
    // commits are not the branch's.
    let since = match (&capped, made) {
        (Some(_), Some(made)) => &since[..made.min(since.len())],
        _ => since,
    };
    Ok(Since {
        fork,
        head: head.to_owned(),
        commits: since.iter().map(ReviewCommit::from).collect(),
        merged_in,
        capped,
    })
}

/// The branch's commits since its fork: those `read` holds from a read started as this one
/// would be (`merged_in`), else read now ([`commits_since_fork`]).
async fn since_of(
    read: &ForkRead,
    merged_in: bool,
    session: &Entity<GraphSession>,
    branch: &str,
    target: &str,
    cx: &mut AsyncApp,
) -> Result<Since, String> {
    match read.since(merged_in) {
        Some(since) => Ok(since),
        None => {
            commits_since_fork(
                session, branch, target, &read.base, &read.head, merged_in, cx,
            )
            .await
        }
    }
}

/// Reads a branch the target was merged into since it forked: its feed holds that merge's
/// block, which carries the target's own changes, so the changes are the net ones from the
/// merge base to the reviewed head (`GET /diff`, pinned to both), up to the change cap. The
/// commits are the branch's own since it forked. The changes do not depend on the commits:
/// both are read at once, and a failure is told as when read in turn, the commits' first.
async fn read_review_since_merge(
    session: Entity<GraphSession>,
    branch: String,
    target: String,
    mut read: ForkRead,
    this: WeakEntity<ReviewView>,
    cx: &mut AsyncApp,
) -> Result<ReviewData, String> {
    let (base, head) = (read.base.clone(), read.head.clone());
    // A range has no single commit: each change is the reviewed head's.
    let cause = ChangeCauseOutput {
        graph_commit_id: head.clone(),
        authored_branch: branch.clone(),
        authored_at: 0,
        parent_commit_id: None,
        merged_parent_commit_id: None,
        actor_id: None,
    };
    let mut since_cx = cx.clone();
    let (since, folded) = futures::join!(
        since_of(&read, true, &session, &branch, &target, &mut since_cx),
        fold_pages(
            &this,
            cx,
            async |token: Option<String>, limit: u32, cx: &mut AsyncApp| {
                let page = session
                    .update(cx, |session, cx| {
                        session.diff(&base, &head, token, limit, cx)
                    })
                    .await
                    .map_err(|e| e.to_string())?;
                let block = ChangeBlockOutput {
                    cause: cause.clone(),
                    changes: page.changes,
                };
                Ok((block, page.next_page_token))
            },
        ),
    );
    let since = since?;
    let (fold, partial) = folded?;
    let fed = Fed {
        fold,
        commits: since.commits.iter().rev().cloned().collect(),
        partial,
        moved_while_reading: false,
    };
    let mut data = fed
        .into_review(&session, Some(base), Some(head), false, cx)
        .await;
    data.merged_in = true;
    data.commits_capped = since.capped.clone();
    read.keep_since(true, since);
    data.fork_read = Some(read);
    Ok(data)
}

/// Reads one commit's own changes, page by page, up to the cap, with the branch's commits since
/// it forked from `target`: both at once, as neither depends on the other, a failure told as
/// when read in turn, the commits' first.
async fn read_commit_review(
    session: Entity<GraphSession>,
    branch: String,
    target: String,
    scope: Option<String>,
    mut read: ForkRead,
    this: WeakEntity<ReviewView>,
    cx: &mut AsyncApp,
) -> Result<ReviewData, String> {
    let Some(commit) = scope else {
        return Err("A review of one commit names the commit.".into());
    };
    let mut since_cx = cx.clone();
    let (since, folded) = futures::join!(
        since_of(&read, false, &session, &branch, &target, &mut since_cx),
        fold_pages(
            &this,
            cx,
            async |token: Option<String>, limit: u32, cx: &mut AsyncApp| {
                let page = session
                    .update(cx, |session, cx| {
                        session.commit_changes(&commit, token, limit, cx)
                    })
                    .await
                    .map_err(|e| e.to_string())?;
                // The page is shared with the session's memo: its changes are copied for the
                // fold off the main thread.
                let (cause, changes) = (page.cause, page.changes);
                let block = cx
                    .background_executor()
                    .spawn(async move {
                        ChangeBlockOutput {
                            cause,
                            changes: Arc::unwrap_or_clone(changes),
                        }
                    })
                    .await;
                Ok((block, page.next_page_token))
            },
        ),
    );
    let since = since?;
    let (fold, partial) = folded?;
    let fed = Fed {
        fold,
        commits: since.commits.iter().rev().cloned().collect(),
        partial,
        moved_while_reading: false,
    };
    // A branch the target was merged into is reviewed since that merge's base, as a review of
    // all its commits is.
    let fork = if since.merged_in {
        Some(read.base.clone())
    } else {
        since.fork.clone()
    };
    let mut data = fed
        .into_review(&session, fork, Some(since.head.clone()), true, cx)
        .await;
    data.merged_in = since.merged_in;
    data.commits_capped = since.capped.clone();
    read.keep_since(false, since);
    data.fork_read = Some(read);
    Ok(data)
}

/// After a lost merge response: whether the target holds the source head the merge was sent for.
async fn merge_landed(
    session: &Entity<GraphSession>,
    source: &str,
    target: &str,
    sent: &MergeSent,
    cx: &mut AsyncApp,
) -> Check {
    if sent.source_head.is_none() {
        return Check::Unread;
    }
    merge_shown(
        session,
        source,
        target,
        None,
        sent.source_head.as_deref(),
        sent.target_head.as_deref(),
        cx,
    )
    .await
}

/// Whether the target shows the merge of `source_head` (or the merge commit `commit`). The source
/// head being the merge base of the two proves it with one read: merged, fast-forwarded, or
/// already there. When the merge base has moved past it (more of the source was merged since),
/// the target's history is read from its head, where a merge just made is, a page at a time
/// for at most [`FORK_PAGES`] pages: the merge commit, or a commit carrying the source head. A
/// merge publishes a commit of the target above the head the target had before it was sent
/// (`before`), so a read that reaches that head without it has read every commit the merge can
/// be ([`Check::NotShown`]). A merge not in the pages read is not confirmed
/// ([`Check::NotReached`]), rather than looked for in the rest of the history.
async fn merge_shown(
    session: &Entity<GraphSession>,
    source: &str,
    target: &str,
    commit: Option<&str>,
    source_head: Option<&str>,
    before: Option<&str>,
    cx: &mut AsyncApp,
) -> Check {
    // The source head being the merge base, or the target at the merge commit, proves it. So
    // does the source's head now being the merge base: the merge took a newer head than the
    // one asked about, and that one's history with it.
    let read = session.update(cx, |session, cx| session.merge_base(source, target, cx));
    if let Ok(base) = read.await
        && (source_head == Some(base.merge_base.graph_commit_id.as_str())
            || commit == Some(base.target_commit_id.as_str())
            || (source_head.is_some() && base.source_commit_id == base.merge_base.graph_commit_id))
    {
        return Check::Shown;
    }
    let shows = |list: &[CommitOutput]| {
        commit.is_some_and(|c| list.iter().any(|t| t.graph_commit_id == c))
            || source_head.is_some_and(|head| contains_merge_of(list, head))
    };
    let mut pages = Pages::new(target);
    while pages.more() {
        let Ok(page) = pages.next(session, cx).await else {
            return Check::Unread;
        };
        if shows(&page) {
            return Check::Shown;
        }
        // Below here is older than the merge.
        if before.is_some_and(|before| page.iter().any(|c| c.graph_commit_id == before)) {
            return Check::NotShown;
        }
    }
    if pages.done {
        Check::NotShown
    } else {
        Check::NotReached
    }
}

impl Render for ReviewView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Drawn, so shown: the dry run owed is read once the frame is out, not inside it.
        if std::mem::take(&mut self.preview_owed) {
            cx.defer_in(window, |this, _, cx| this.preview_moved(cx));
        }
        let header = self.render_header(cx);
        let body = match (&self.load, &self.data) {
            (LoadState::Ready, Some(_)) if self.show_conflicts => self.render_conflicts(cx),
            (LoadState::Ready, Some(data)) if data.folded.changes.is_empty() => {
                studio_ui::ds::empty_state(IconName::Check, "Nothing to review")
                    .child(studio_ui::ds::state_detail(if data.commits.is_empty() {
                        format!("{} has no commits of its own.", self.branch)
                    } else {
                        format!(
                            "Its commits change nothing against {} in the end.",
                            self.target
                        )
                    }))
                    .into_any_element()
            }
            (LoadState::Ready, Some(_)) => list(
                self.list_state.clone(),
                cx.processor(|this, row, window, cx| this.render_row(row, window, cx)),
            )
            .flex_1()
            .min_h_0()
            .py(px(space::MD))
            .into_any_element(),
            (LoadState::Failed(error), _) => {
                studio_ui::ds::error_state("The review could not be read", error.clone())
                    .into_any_element()
            }
            _ => studio_ui::ds::loading_state("Reading changes…").into_any_element(),
        };
        // Typing in the search box must not move between cards.
        let mut context = gpui::KeyContext::new_with_defaults();
        context.add("Review");
        if self.search.focus_handle(cx).is_focused(window) {
            context.add("searching");
        }
        v_flex()
            .key_context(context)
            .track_focus(&self.focus_handle)
            .size_full()
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::accept_and_next))
            // Revert and Edit are registered only while the credential may change the branch,
            // as their buttons are enabled (constitution VIII).
            .when(self.change_refusal(cx).is_none(), |view| {
                view.on_action(cx.listener(Self::revert_and_next))
                    .on_action(cx.listener(|this, _: &EditChange, window, cx| {
                        if this.cards_shown()
                            && let Some(ix) = this.selected
                        {
                            this.open_edit(ix, window, cx);
                        }
                    }))
            })
            // Merge is registered only while the credential may merge (constitution VIII).
            .when(self.merge_refusal(cx).is_none(), |view| {
                view.on_action(cx.listener(|this, _: &Merge, _, cx| this.merge(cx)))
            })
            // ⌘R reads the review again, as its Refresh buttons do, and goes on to the
            // workspace, which reads the graph's schema, branches and counts again.
            .on_action(cx.listener(|this, _: &Refresh, _, cx| {
                this.reload(cx);
                cx.propagate();
            }))
            .child(header)
            .child(body)
    }
}

/// A Refresh button's tooltip, which shows the key that does the same in the review.
fn refresh_tooltip(
    title: &'static str,
    focus: &FocusHandle,
) -> impl Fn(&mut Window, &mut App) -> gpui::AnyView + use<> {
    let focus = focus.clone();
    move |_, cx| Tooltip::for_action_in(title, &Refresh, &focus, cx)
}

/// Opens the review of `branch` against `target`. A merge starts there, from a head the person
/// has seen: opening never merges.
pub fn open_review(
    workspace: &mut Workspace,
    branch: String,
    target: String,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(session) = workspace.session().cloned() else {
        return;
    };
    if branch == target {
        workspace.show_toast(
            studio_workspace::ToastKind::Info,
            format!("You are on {target}. Switch to another branch to review it against {target}."),
            cx,
        );
        return;
    }
    let key = review_key(&branch, &target);
    workspace.open_item::<ReviewView>(key, window, cx, |window, cx| {
        ReviewView::new(session, branch, target, window, cx)
    });
}

#[cfg(test)]
pub(crate) mod tests {
    use graph_client::api::{ChangeImageOutput, EntityKind};
    use serde_json::json;

    use super::*;

    fn change(id: &str, after: Value) -> FoldedChange {
        FoldedChange {
            kind: EntityKind::Node,
            type_id: "t-Person".into(),
            type_name: "Person".into(),
            id: id.into(),
            before: None,
            after: Some(ChangeImageOutput {
                properties: after.as_object().unwrap().clone(),
                endpoints: None,
            }),
            commits: vec!["c1".into()],
        }
    }

    #[test]
    fn search_matches_ids_types_and_values_case_blind() {
        let c = change(
            "person-000001",
            json!({"name": "Donald Dijkstra", "born": 1985}),
        );
        let text = search_text_of(&c);
        for found in ["000001", "person", "dijkstra", "1985"] {
            assert!(text.contains(found), "{found}");
        }
        assert!(!text.contains("lovelace"));
    }

    /// A card shows when its own change or a relationship on it matches the search, the first
    /// run of a section carries its title, and each header counts what its run shows.
    #[test]
    fn a_search_keeps_the_cards_whose_changes_match_and_headers_count_what_shows() {
        let schema = SchemaModel::parse(
            "node Person {\n    slug: String @key\n    name: String?\n}\n\
             node Note {\n    slug: String @key\n    title: String?\n}\n\
             edge About: Note -> Person\n",
        )
        .unwrap();
        let mut note = change("lunch", json!({"slug": "lunch", "title": "Lunch notes"}));
        note.type_id = "t-Note".into();
        note.type_name = "Note".into();
        let mut about = change("e1", json!({}));
        about.kind = EntityKind::Edge;
        about.type_id = "t-About".into();
        about.type_name = "About".into();
        about.after.as_mut().unwrap().endpoints = Some(json!({"from": "lunch", "to": "maya"}));
        let folded = FoldedChanges {
            changes: vec![
                change("maya", json!({"slug": "maya", "name": "Ada"})),
                note,
                about,
            ],
            raw_changes: 3,
        };
        let groups = EntityGroups::new(&folded, Some(&schema));
        let folded = Arc::new(folded);
        let data = ReviewData {
            fork: None,
            head: None,
            commits: Vec::new().into(),
            fork_read: None,
            search: SearchIndex::new(folded.clone()),
            counts: folded.counts(),
            revert_blocked: vec![None; 3],
            by_entity: entity_order(&folded.changes),
            by_key: HashMap::new(),
            card_rows: CardRows::default(),
            marks: folded.changes.iter().map(mark_of).collect(),
            schema: None,
            folded,
            groups,
            partial: false,
            moved_while_reading: false,
            merged_in: false,
            commits_capped: None,
        };
        let shown = |query: &str| {
            let matched = data.search.matches(query);
            let (rows, headers) = build_rows(&data, matched.as_deref());
            rows.iter()
                .map(|row| match row {
                    Row::Header(ix) => {
                        let h = &headers[*ix];
                        format!(
                            "{}{} +{} r{}",
                            if h.section.is_some() { "§ " } else { "" },
                            h.type_name,
                            h.added,
                            h.relationships
                        )
                    }
                    Row::Entity(ix) => data.groups.groups[*ix].id.clone(),
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(shown(""), ["§ Person +1 r0", "maya", "Note +1 r1", "lunch"]);
        assert_eq!(shown("ada"), ["§ Person +1 r0", "maya"]);
        // A section's title goes with its first run that shows.
        assert_eq!(shown("lunch notes"), ["§ Note +1 r1", "lunch"]);
        // An end of a relationship finds the card it is reviewed on.
        assert_eq!(
            shown("maya"),
            ["§ Person +1 r0", "maya", "Note +1 r1", "lunch"]
        );
        // A conflict finds its entity's change by type and id.
        assert_eq!(data.change_to("Person", "maya"), Some(0));
        assert_eq!(data.change_to("Note", "lunch"), Some(1));
        assert_eq!(data.change_to("About", "e1"), Some(2));
        assert_eq!(data.change_to("Person", "lunch"), None);
    }

    /// A reviewed mark holds for the change as it was reviewed, read from any commit: changed
    /// again since, it is reviewed again.
    #[test]
    fn a_reviewed_mark_is_kept_by_the_change_as_it_was_reviewed() {
        let reviewed = change("p1", json!({"name": "Ada"}));
        let mut read_again = reviewed.clone();
        read_again.commits = vec!["r1".into()];
        assert_eq!(
            mark_of(&reviewed),
            mark_of(&read_again),
            "another head, same change"
        );
        let again = change("p1", json!({"name": "Ada Lovelace"}));
        assert_ne!(mark_of(&reviewed), mark_of(&again), "changed again");
        assert_ne!(
            mark_of(&reviewed),
            mark_of(&change("p2", json!({"name": "Ada"}))),
            "another entity"
        );
    }

    /// Fixes asked for one after the other go one at a time, each bound to the commit the one
    /// before made, and the merge waits for them.
    #[gpui::test]
    async fn fixes_go_one_at_a_time_each_bound_to_the_head_the_one_before_left(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let (session, asked) = reviewed_server_writing(false, cx);
        let (view, cx) = cx.add_window_view(|window, cx| {
            ReviewView::new(session, "feature".into(), "main".into(), window, cx)
        });
        cx.run_until_parked();
        asked.lock().unwrap().clear();
        view.update(cx, |view, cx| {
            assert!(matches!(view.load, LoadState::Ready));
            view.revert(0, cx);
            view.revert(1, cx);
            assert_eq!(view.fixes_running, 2);
            view.merge(cx);
            assert!(
                matches!(view.merge, MergeState::Idle),
                "no merge while fixes are out"
            );
        });
        cx.run_until_parked();
        let bound: Vec<String> = asked
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.contains("/mutate"))
            .map(|request| request.rsplit(' ').next().unwrap().to_owned())
            .collect();
        assert_eq!(bound, ["f2", "r1"]);
        // The dry run was for the head before the fixes: it is read once more after them.
        let dry_runs = asked
            .lock()
            .unwrap()
            .iter()
            .filter(|request| {
                request.contains("/branches/merge") && request.contains("\"dry_run\":true")
            })
            .count();
        assert_eq!(dry_runs, 1, "{:?}", asked.lock().unwrap());
        view.read_with(cx, |view, _| {
            assert_eq!(view.fixes_running, 0);
            assert_eq!(view.bound_head.as_deref(), Some("r2"));
            for card in &view.cards {
                assert!(
                    matches!(card.fix, FixState::Reverted { .. }),
                    "{:?}",
                    card.fix
                );
            }
        });
    }

    /// The keys act on cards on screen only: not under "Reading changes…" while a Refresh
    /// reads, nor under the conflicts list, where ⌥⌘Z would revert a card nobody sees.
    #[gpui::test]
    async fn keys_act_only_on_the_cards_shown(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let (session, asked) = reviewed_server_writing(false, cx);
        let (view, cx) = cx.add_window_view(|window, cx| {
            ReviewView::new(session, "feature".into(), "main".into(), window, cx)
        });
        cx.run_until_parked();
        asked.lock().unwrap().clear();
        for hide in [
            |view: &mut ReviewView| view.load = LoadState::Loading { folded: 0 },
            |view: &mut ReviewView| view.show_conflicts = true,
        ] {
            view.update_in(cx, |view, window, cx| {
                hide(view);
                view.revert_and_next(&RevertAndNext, window, cx);
                view.accept_and_next(&AcceptAndNext, window, cx);
                assert_eq!(view.fixes_running, 0);
                assert!(view.cards.iter().all(|card| !card.reviewed));
                view.load = LoadState::Ready;
                view.show_conflicts = false;
            });
        }
        cx.run_until_parked();
        assert!(
            !asked
                .lock()
                .unwrap()
                .iter()
                .any(|request| request.contains("/mutate"))
        );
    }

    /// The second half of a moved relationship waits for the first and is not sent when the
    /// first does not land: the old relationship is not restored with the new one still there.
    #[gpui::test]
    async fn the_second_half_of_a_move_is_not_sent_when_the_first_does_not_land(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let (session, asked) = reviewed_server_writing(false, cx);
        let (view, cx) = cx.add_window_view(|window, cx| {
            ReviewView::new(session, "feature".into(), "main".into(), window, cx)
        });
        cx.run_until_parked();
        asked.lock().unwrap().clear();
        view.update(cx, |view, cx| {
            let first = view.revert_fix(0, cx).unwrap();
            let mut second = view.revert_fix(1, cx).unwrap();
            second.after = Some(first.key.clone());
            // The first is out, the second waits for it.
            view.fixes_running = 2;
            view.fix_out = true;
            view.cards[0].fix = FixState::Running;
            view.cards[1].fix = FixState::Running;
            view.fixes_in_flight
                .insert(second.key.clone(), FixState::Running);
            view.fix_queue.push_back(second);
            view.finish_fix(
                &first.key,
                WriteOutcome::Unknown {
                    reason: "the response was lost".into(),
                },
                false,
                cx,
            );
            assert!(view.fix_queue.is_empty());
            assert_eq!(view.fixes_running, 0);
            assert!(view.fixes_in_flight.is_empty());
            // The first's outcome is unknown, and the second says so: not that it was not
            // removed.
            assert!(
                matches!(&view.cards[1].fix, FixState::Refused(why) if why.contains("is not known")),
                "{:?}",
                view.cards[1].fix
            );
        });
        cx.run_until_parked();
        assert!(
            !asked
                .lock()
                .unwrap()
                .iter()
                .any(|request| request.contains("/mutate"))
        );
    }

    /// An edit that cannot be sent says why, for the form to show, rather than closing with
    /// nothing written: during a merge, or once the review no longer shows the change.
    #[gpui::test]
    async fn an_edit_not_sent_says_why(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let (session, _) = reviewed_server_writing(false, cx);
        let (view, cx) = cx.add_window_view(|window, cx| {
            ReviewView::new(session, "feature".into(), "main".into(), window, cx)
        });
        cx.run_until_parked();
        view.update(cx, |view, cx| {
            let key = view.data.as_ref().unwrap().folded.changes[0].key();
            let edit = || vec![("name".to_owned(), json!("x"), json!("y"))];
            view.merge = MergeState::Merging;
            assert_eq!(view.save_edit(&key, edit(), cx), Err(MERGE_RUNNING.into()));
            view.merge = MergeState::Idle;
            assert!(view.save_edit("Person/gone", edit(), cx).is_err());
            assert_eq!(view.fixes_running, 0);
        });
    }

    /// A Refresh that lands while a fix is out keeps the fix's card showing it: Revert is not
    /// offered again, and the fix is sent once.
    #[gpui::test]
    async fn a_refresh_while_a_fix_is_out_does_not_offer_it_again(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let (session, asked) = reviewed_server_writing(false, cx);
        let (view, cx) = cx.add_window_view(|window, cx| {
            ReviewView::new(session, "feature".into(), "main".into(), window, cx)
        });
        cx.run_until_parked();
        asked.lock().unwrap().clear();
        view.update(cx, |view, cx| {
            view.revert(0, cx);
            // The review read again lands before the fix is back.
            let data = view.data.take().unwrap();
            view.finish_load(Ok(data), Searched::default(), cx);
            assert!(view.cards[0].fix.done(), "{:?}", view.cards[0].fix);
            view.revert(0, cx);
            assert_eq!(view.fixes_running, 1, "not asked for again");
        });
        cx.run_until_parked();
        let writes = asked
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.contains("/mutate"))
            .count();
        assert_eq!(writes, 1);
        view.read_with(cx, |view, _| {
            assert!(matches!(view.cards[0].fix, FixState::Reverted { .. }));
        });
    }

    /// A merge that took a commit of the branch made after Studio's check says so: what landed
    /// is more than was reviewed. A fast-forward publishes a new commit of the target whose
    /// merged parent is the head it took, as a merge commit does: one of the reviewed head is
    /// not said to take more.
    #[gpui::test]
    async fn a_merge_of_a_head_newer_than_the_review_says_so(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        for (outcome, took, beyond) in [
            ("merged", "f2", false),
            ("merged", "f3", true),
            ("fast_forward", "f2", false),
            ("fast_forward", "f3", true),
        ] {
            // The merge's answer: a new commit of main, m3, whose merged parent is `took`.
            let merge = move |path: &str, _: &str, sent: &str| {
                (path.ends_with("/branches/merge") && !sent.contains(DRY_RUN)).then(|| {
                    let commit = json!({ "graph_commit_id": "m3", "graph_manifest_version": 4, "kind": "published", "created_at": 0, "parent_commit_id": "m2", "merged_parent_commit_id": took });
                    (
                        200,
                        json!({ "source": "feature", "target": "main", "outcome": outcome, "commit": commit }),
                    )
                })
            };
            let (session, _) =
                review_server_with(None, false, None, took, |_| (500, json!({})), merge, cx);
            let (view, cx) = cx.add_window_view(|window, cx| {
                ReviewView::new(session, "feature".into(), "main".into(), window, cx)
            });
            cx.run_until_parked();
            view.update(cx, |view, cx| view.merge(cx));
            cx.run_until_parked();
            view.read_with(cx, |view, _| match &view.merge {
                MergeState::Merged { beyond_review, .. } => {
                    assert_eq!(*beyond_review, beyond, "{outcome} of {took}")
                }
                _ => panic!("not merged"),
            });
        }
    }

    /// What a lost merge was sent for: `head`, over a target head not known.
    fn sent(head: &str) -> MergeSent {
        MergeSent {
            source_head: Some(head.into()),
            target_head: None,
        }
    }

    /// A merge request's body when it is a dry run.
    const DRY_RUN: &str = "\"dry_run\":true";

    /// A merge whose response was lost keeps Merge, and every fix, off while main's history is
    /// read for it, and while what was read does not settle it: a second merge would cancel that
    /// read and be sent, and a fix could land on either side of it, while the first may still
    /// land. A Refresh reads the history again, never the merge. A review of the same branches
    /// opened again starts from the lost merge and reads for it too, so closing the tab does not
    /// offer Merge again. History read to its end without the merge lets Merge and the fixes run
    /// again, saying the first may still land; a Refresh after that forgets it.
    #[gpui::test]
    async fn a_lost_merge_keeps_merge_off_until_history_settles_it(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let readable = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let main_readable = readable.clone();
        // The merge's answer is lost at a gateway; main's history cannot be read at first.
        let lost = move |path: &str, query: &str, sent: &str| {
            if path.ends_with("/branches/merge") && !sent.contains(DRY_RUN) {
                Some((504, json!({})))
            } else if path.ends_with("/commits")
                && query.contains("branch=main")
                && !main_readable.load(std::sync::atomic::Ordering::SeqCst)
            {
                Some((500, json!({ "error": "down" })))
            } else {
                None
            }
        };
        let (session, asked) =
            review_server_with(None, false, None, "f2", |_| (500, json!({})), lost, cx);
        let open = |cx: &mut gpui::TestAppContext| {
            let session = session.clone();
            let (view, cx) = cx.add_window_view(|window, cx| {
                ReviewView::new(session, "feature".into(), "main".into(), window, cx)
            });
            cx.run_until_parked();
            (view, cx.clone())
        };
        let (view, mut window) = open(cx);
        let cx = &mut window;
        let count = |route: &dyn Fn(&String) -> bool| {
            asked.lock().unwrap().iter().filter(|r| route(r)).count()
        };
        let merges = || count(&|r| r.contains("/branches/merge?") && !r.contains(DRY_RUN));
        let main_reads = || count(&|r| r.contains("/commits?branch=main"));
        let writes = || count(&|r| r.contains("/mutate"));

        // While main's history is read for it, the merge is out.
        view.update(cx, |view, cx| {
            view.merge = MergeState::Unknown {
                reason: "the server answered 504".into(),
                landed: Check::Reading,
                sent: MergeSent::default(),
            };
            assert!(view.merging());
            assert!(!view.mergeable(cx));
            view.merge(cx);
            assert!(matches!(
                view.merge,
                MergeState::Unknown {
                    landed: Check::Reading,
                    ..
                }
            ));
            // A Refresh meanwhile leaves the read out, and Merge off.
            view.reload(cx);
            assert!(!view.mergeable(cx));
            view.merge = MergeState::Idle;
        });
        cx.run_until_parked();

        view.update(cx, |view, cx| view.merge(cx));
        cx.run_until_parked();
        assert_eq!(merges(), 1);
        view.update(cx, |view, cx| {
            assert!(
                matches!(
                    &view.merge,
                    MergeState::Unknown {
                        landed: Check::Unread,
                        sent,
                        ..
                    } if sent.source_head.as_deref() == Some("f2")
                        && sent.target_head.as_deref() == Some("m2")
                ),
                "{:?}",
                view.merge
            );
            assert!(!view.mergeable(cx));
            assert_eq!(
                view.merge_blocked(cx).as_deref(),
                Some("The merge sent before may have landed: Refresh to read main's history again")
            );
            view.merge(cx);
            // Nor a fix: it could land on either side of the merge.
            view.revert(0, cx);
            assert_eq!(view.cards[0].fix, FixState::Refused(MERGE_RUNNING.into()));
            assert_eq!(view.fixes_running, 0);
        });
        cx.run_until_parked();
        assert_eq!(merges(), 1, "not sent again");
        assert_eq!(writes(), 0);

        // The review opened again, as after its tab was closed: it starts from the lost merge,
        // reads main's history for it, and offers neither Merge nor a fix.
        let before = main_reads();
        let (again, mut again_window) = open(cx);
        let again_cx = &mut again_window;
        again.update(again_cx, |again, cx| {
            assert!(
                matches!(again.merge, MergeState::Unknown { .. }),
                "{:?}",
                again.merge
            );
            assert!(!again.mergeable(cx));
            again.merge(cx);
            again.revert(0, cx);
            assert_eq!(again.fixes_running, 0);
        });
        again_cx.run_until_parked();
        assert!(main_reads() > before, "read for the merge again");
        assert_eq!(merges(), 1, "not sent again");
        assert_eq!(writes(), 0);

        // Readable now, and without the merge: a Refresh reads it, and Merge runs again.
        readable.store(true, std::sync::atomic::Ordering::SeqCst);
        let before = main_reads();
        view.update(cx, |view, cx| view.reload(cx));
        cx.run_until_parked();
        assert!(main_reads() > before, "main's history read again");
        assert_eq!(merges(), 1, "not sent again");
        view.read_with(cx, |view, cx| {
            assert!(
                matches!(
                    view.merge,
                    MergeState::Unknown {
                        landed: Check::NotShown,
                        ..
                    }
                ),
                "{:?}",
                view.merge
            );
            assert!(view.mergeable(cx));
            assert!(!view.merge_may_land(cx), "fixes are offered again");
        });
        assert_eq!(
            unknown_merge_words("the server answered 504", Check::NotShown, "main"),
            "The merge response was lost (the server answered 504) and main's history does not \
             show it yet, but the merge may still land. Studio did not retry; open History to \
             check before merging again."
        );
        // Read again after that, the review forgets it, and so does one opened since.
        view.update(cx, |view, cx| view.reload(cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(matches!(view.merge, MergeState::Idle))
        });
        let (third, third_window) = open(cx);
        third.read_with(&third_window, |third, cx| {
            assert!(matches!(third.merge, MergeState::Idle));
            assert!(third.mergeable(cx));
        });
    }

    /// A merge that lands publishes a commit of main above the head main had when Studio checked
    /// the branch before sending it, so a lost merge is looked for only down to that head: on a
    /// main of more than 10,000 commits, one that never landed reads one page and is not shown,
    /// rather than 20 pages and "not reached", which would keep Merge off for good.
    #[gpui::test]
    async fn a_lost_merge_is_looked_for_only_above_the_head_it_was_sent_over(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let main = chain_of("m", FORK_BOUND + 501, None);
        let base = json!({ "merge_base": listed("m1", None, None), "source": "feature", "source_commit_id": "f2", "target": "main", "target_commit_id": "m10500" });
        let lost = move |path: &str, query: &str, sent: &str| {
            if path.ends_with("/branches/merge") && !sent.contains(DRY_RUN) {
                Some((504, json!({})))
            } else if path.ends_with("/branches/merge-base") {
                Some((200, base.clone()))
            } else if path.ends_with("/commits") && query.contains("branch=main") {
                Some((200, page_of(&main, query)))
            } else {
                None
            }
        };
        let (session, asked) =
            review_server_with(None, false, None, "f2", |_| (500, json!({})), lost, cx);
        let (view, cx) = cx.add_window_view(|window, cx| {
            ReviewView::new(session, "feature".into(), "main".into(), window, cx)
        });
        cx.run_until_parked();
        view.update(cx, |view, cx| view.merge(cx));
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert!(
                matches!(
                    view.merge,
                    MergeState::Unknown {
                        landed: Check::NotShown,
                        ..
                    }
                ),
                "{:?}",
                view.merge
            );
            assert!(view.mergeable(cx));
        });
        assert_eq!(listings(&asked, "main"), 1);
    }

    /// A server whose answer to the first merge of feature into main is lost at a gateway, and
    /// whose main history cannot be read until `readable` is set. Later merges are answered.
    fn lost_merge_server(
        readable: std::sync::Arc<std::sync::atomic::AtomicBool>,
        cx: &mut gpui::TestAppContext,
    ) -> (
        Entity<GraphSession>,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        let answered = std::sync::atomic::AtomicBool::new(false);
        let lost = move |path: &str, query: &str, sent: &str| {
            use std::sync::atomic::Ordering::SeqCst;
            if path.ends_with("/branches/merge") && !sent.contains(DRY_RUN) {
                (!answered.swap(true, SeqCst)).then(|| (504, json!({})))
            } else if path.ends_with("/commits")
                && query.contains("branch=main")
                && !readable.load(SeqCst)
            {
                Some((500, json!({ "error": "down" })))
            } else {
                None
            }
        };
        review_server_with(None, false, None, "f2", |_| (500, json!({})), lost, cx)
    }

    /// The session's outcome log is the one record of a lost merge: the review that sent it
    /// closed, a review of the same branches opened afterwards starts from the log's entry
    /// alone, which names the merge's source and target, the heads it was sent over and its
    /// unknown outcome, reads main's history for those heads, and offers neither Merge nor a fix.
    #[gpui::test]
    async fn a_review_opened_after_the_one_that_merged_closed_starts_from_the_log(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let readable = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (session, asked) = lost_merge_server(readable, cx);
        let count = |route: &dyn Fn(&String) -> bool| {
            asked.lock().unwrap().iter().filter(|r| route(r)).count()
        };
        let merges = || count(&|r| r.contains("/branches/merge?") && !r.contains(DRY_RUN));
        let main_reads = || count(&|r| r.contains("/commits?branch=main"));
        let writes = || count(&|r| r.contains("/mutate"));

        let first = {
            let session = session.clone();
            let (view, window) = cx.add_window_view(|window, cx| {
                ReviewView::new(session, "feature".into(), "main".into(), window, cx)
            });
            window.run_until_parked();
            view.update(window, |view, cx| view.merge(cx));
            window.run_until_parked();
            view.read_with(window, |view, _| {
                assert!(
                    matches!(
                        view.merge,
                        MergeState::Unknown {
                            landed: Check::Unread,
                            ..
                        }
                    ),
                    "{:?}",
                    view.merge
                );
            });
            // Its tab closed: the window goes, and the review with it.
            let first = view.downgrade();
            drop(view);
            window.update(|window, _| window.remove_window());
            window.run_until_parked();
            first
        };
        assert!(first.upgrade().is_none(), "the first review is gone");
        assert_eq!(merges(), 1);

        let sent = MergeSent {
            source_head: Some("f2".into()),
            target_head: Some("m2".into()),
        };
        session.read_with(cx, |session, _| {
            let entry = session.newest_merge("feature", "main").unwrap();
            assert_eq!(entry.branch, "main");
            assert!(
                matches!(&entry.outcome, OutcomeKind::Unknown { reason } if reason.contains("504")),
                "{:?}",
                entry.outcome
            );
            assert_eq!(
                entry.merge,
                Some(graph_store::MergeEntry {
                    source: "feature".into(),
                    sent: sent.clone(),
                    settled: None,
                })
            );
        });

        let before = main_reads();
        let (again, again_cx) = cx.add_window_view(|window, cx| {
            ReviewView::new(session, "feature".into(), "main".into(), window, cx)
        });
        again_cx.run_until_parked();
        again.update(again_cx, |again, cx| {
            assert!(
                matches!(
                    &again.merge,
                    MergeState::Unknown {
                        landed: Check::Unread,
                        sent: held,
                        ..
                    } if *held == sent
                ),
                "{:?}",
                again.merge
            );
            assert!(!again.mergeable(cx));
            again.merge(cx);
            again.revert(0, cx);
            assert_eq!(again.cards[0].fix, FixState::Refused(MERGE_RUNNING.into()));
            assert_eq!(again.fixes_running, 0);
        });
        again_cx.run_until_parked();
        assert!(main_reads() > before, "main's history read for the merge");
        assert_eq!(merges(), 1, "not sent again");
        assert_eq!(writes(), 0);
    }

    /// What one review's read of main's history shows of a lost merge is recorded in the
    /// session's log, and every other review of the same branches takes it from there: one open
    /// since before the merge, held only by the log, and one opened after it, whose own read could
    /// not tell. Once the first review's read shows main without the merge, both offer Merge and
    /// the fixes again without a Refresh of their own, and the second's merge is sent.
    #[gpui::test]
    async fn settling_a_lost_merge_in_one_review_lets_another_merge(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let readable = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (session, asked) = lost_merge_server(readable.clone(), cx);
        let merges = || {
            asked
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r.contains("/branches/merge?") && !r.contains(DRY_RUN))
                .count()
        };
        let open = |cx: &mut gpui::TestAppContext| {
            let session = session.clone();
            let (view, cx) = cx.add_window_view(|window, cx| {
                ReviewView::new(session, "feature".into(), "main".into(), window, cx)
            });
            cx.run_until_parked();
            (view, cx.clone())
        };
        let (earlier, mut earlier_window) = open(cx);
        let (sender, mut sender_window) = open(cx);
        sender.update(&mut sender_window, |sender, cx| sender.merge(cx));
        sender_window.run_until_parked();
        assert_eq!(merges(), 1);

        // Open since before the merge, the review holds nothing of it: the log keeps Merge off.
        earlier.update(&mut earlier_window, |earlier, cx| {
            assert!(
                matches!(earlier.merge, MergeState::Idle),
                "{:?}",
                earlier.merge
            );
            assert_eq!(
                earlier.merge_blocked(cx).as_deref(),
                Some(
                    "A merge of feature into main sent before may have landed: Refresh to read \
                     main's history again"
                )
            );
            earlier.revert(0, cx);
            assert_eq!(
                earlier.cards[0].fix,
                FixState::Refused(MERGE_RUNNING.into())
            );
        });
        let (later, mut later_window) = open(cx);
        later.read_with(&later_window, |later, cx| {
            assert!(
                matches!(
                    later.merge,
                    MergeState::Unknown {
                        landed: Check::Unread,
                        ..
                    }
                ),
                "{:?}",
                later.merge
            );
            assert!(!later.mergeable(cx));
        });

        // The sender's Refresh reads main to the head it was before the merge, without it.
        readable.store(true, std::sync::atomic::Ordering::SeqCst);
        sender.update(&mut sender_window, |sender, cx| sender.reload(cx));
        sender_window.run_until_parked();
        session.read_with(cx, |session, _| {
            let entry = session.newest_merge("feature", "main").unwrap();
            assert_eq!(
                entry.merge.as_ref().unwrap().settled,
                Some(MergeSettled::NotShown)
            );
        });
        earlier.read_with(&earlier_window, |earlier, cx| {
            assert!(earlier.mergeable(cx));
            assert_eq!(earlier.cards[0].fix, FixState::Idle, "offered again");
        });
        later.read_with(&later_window, |later, cx| {
            assert!(
                matches!(
                    later.merge,
                    MergeState::Unknown {
                        landed: Check::NotShown,
                        ..
                    }
                ),
                "{:?}",
                later.merge
            );
            assert!(later.mergeable(cx));
            assert!(!later.merge_may_land(cx));
        });

        later.update(&mut later_window, |later, cx| later.merge(cx));
        later_window.run_until_parked();
        assert_eq!(merges(), 2, "merged from the other review");
        later.read_with(&later_window, |later, _| {
            assert!(
                matches!(later.merge, MergeState::Merged { .. }),
                "{:?}",
                later.merge
            );
        });
    }

    /// A credential the access route refuses changes on the branch reverts and edits nothing
    /// from the review: Revert and Edit say why, their keys (⌥⌘Z, ↵) are not registered, and
    /// neither a revert nor an edit is sent. The review is still read, and marked.
    #[gpui::test]
    async fn a_change_the_credential_may_not_make_is_not_offered(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let access = json!({ "graph_id": "knowledge", "actor_id": "act-dev", "graph_wide": ["read"], "actions": ["read", "branch_merge"] });
        let (session, asked) = review_server_with(
            Some(access),
            false,
            None,
            "f2",
            |_| (500, json!({})),
            |_, _, _| None,
            cx,
        );
        let (view, cx) = cx.add_window_view(|window, cx| {
            ReviewView::new(session, "feature".into(), "main".into(), window, cx)
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.focus(&view.focus_handle(cx), cx));
        cx.run_until_parked();
        assert!(!cx.update(|window, cx| window.is_action_available(&RevertAndNext, cx)));
        assert!(!cx.update(|window, cx| window.is_action_available(&EditChange, cx)));
        assert!(cx.update(|window, cx| window.is_action_available(&AcceptAndNext, cx)));
        let refused = "Your credential may not change feature";
        view.update_in(cx, |view, window, cx| {
            assert!(matches!(view.load, LoadState::Ready));
            assert_eq!(view.change_refusal(cx).as_deref(), Some(refused));
            view.revert(0, cx);
            view.revert_line(1, cx);
            assert_eq!(view.fixes_running, 0);
            assert_eq!(view.cards[0].fix, FixState::Idle);
            let key = view.data.as_ref().unwrap().folded.changes[1].key();
            let edit = vec![("name".to_owned(), json!("x"), json!("y"))];
            assert_eq!(view.save_edit(&key, edit, cx), Err(refused.into()));
            view.open_edit(1, window, cx);
            view.accept_and_next(&AcceptAndNext, window, cx);
            assert!(
                view.cards.iter().any(|card| card.reviewed),
                "marking writes nothing"
            );
        });
        cx.run_until_parked();
        assert!(
            !asked
                .lock()
                .unwrap()
                .iter()
                .any(|request| request.contains("/mutate"))
        );
    }

    /// A new token may see less than the one the review was read with: nothing read under the
    /// old one shows while the review is read again under the new one, not the cards, the
    /// counts, the names, the dry run or the scope menu's commits, and none of it comes back
    /// when the new token is refused the read. A scope change then reads the merge base again,
    /// rather than take the one the old token read. Reviewed marks are the person's, and stay.
    #[gpui::test]
    async fn a_new_credential_reads_the_review_again(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let refused = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let new_token = refused.clone();
        // The new token is refused what the old one read.
        let over = move |path: &str, _: &str, _: &str| {
            (new_token.load(std::sync::atomic::Ordering::SeqCst)
                && path.ends_with("/branches/merge-base"))
            .then(|| (403, json!({ "error": "forbidden", "code": "forbidden", "disposition": { "action": "refresh", "effects": "none" } })))
        };
        let (session, asked) =
            review_server_with(None, false, None, "f2", |_| (500, json!({})), over, cx);
        let (view, cx) = cx.add_window_view({
            let session = session.clone();
            |window, cx| ReviewView::new(session, "feature".into(), "main".into(), window, cx)
        });
        cx.run_until_parked();
        let mark = view.update(cx, |view, cx| {
            assert!(matches!(view.preview, Preview::Merges(_)));
            view.names.insert(
                ("Person".into(), "p9".into()),
                "Read with the old token".into(),
            );
            view.toggle_reviewed(0, cx);
            let reviewed = view.cards.iter().position(|card| card.reviewed).unwrap();
            view.data.as_ref().unwrap().marks[reviewed]
        });
        refused.store(true, std::sync::atomic::Ordering::SeqCst);
        asked.lock().unwrap().clear();
        session.update(cx, |_, cx| cx.emit(SessionEvent::CredentialChanged));
        let gone = |view: &ReviewView| {
            assert!(view.data.is_none(), "the old token's review");
            assert!(view.cards.is_empty() && view.rows.is_empty());
            assert!(
                view.scope_menu_input().0.is_empty(),
                "the old token's commits"
            );
            assert!(view.names.is_empty());
            assert!(matches!(view.preview, Preview::Unread));
            assert!(view.bound_head.is_none());
        };
        view.read_with(cx, |view, _| {
            assert!(matches!(view.load, LoadState::Loading { .. }));
            assert!(view.reviewed_keys.contains(&mark), "the person's mark");
            gone(view);
        });
        cx.run_until_parked();
        let asked_now = std::mem::take(&mut *asked.lock().unwrap());
        assert!(
            asked_now.iter().any(|r| r.contains("/branches/merge-base")),
            "{asked_now:?}"
        );
        view.read_with(cx, |view, cx| {
            assert!(matches!(view.load, LoadState::Failed(_)), "refused");
            gone(view);
            assert!(view.held_fork_read(cx).is_none());
        });

        // Allowed again: a scope change reads the merge base, not the old token's.
        refused.store(false, std::sync::atomic::Ordering::SeqCst);
        view.update(cx, |view, cx| view.set_scope(Some("f1".into()), cx));
        cx.run_until_parked();
        let asked_now = std::mem::take(&mut *asked.lock().unwrap());
        assert!(
            asked_now.iter().any(|r| r.contains("/branches/merge-base")),
            "{asked_now:?}"
        );
        view.read_with(cx, |view, _| assert!(matches!(view.load, LoadState::Ready)));
    }

    /// A scope change at the heads the review was read at takes the merge base and the commits
    /// since the fork it holds: choosing a commit reads its changes beside the branch's history,
    /// never the merge base; choosing another, or going back to every commit, reads neither.
    /// The commits are those a fresh read lists. A head seen moving reads the base again.
    #[gpui::test]
    async fn a_scope_change_at_the_same_heads_reads_no_base_again(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let (session, asked) = reviewed_server(false, cx);
        let (view, cx) = cx.add_window_view({
            let session = session.clone();
            |window, cx| ReviewView::new(session, "feature".into(), "main".into(), window, cx)
        });
        cx.run_until_parked();
        let take = || std::mem::take(&mut *asked.lock().unwrap());
        let ids = |view: &ReviewView| -> Vec<String> {
            let data = view.data.as_ref().unwrap();
            data.commits.iter().map(|c| c.id.clone()).collect()
        };
        let bases = |asked: &[String]| asked.iter().filter(|r| r.contains("/merge-base")).count();
        let listings = |asked: &[String]| asked.iter().filter(|r| r.contains("/commits?")).count();
        take();

        view.update(cx, |view, cx| view.set_scope(Some("f1".into()), cx));
        cx.run_until_parked();
        let asked_now = take();
        assert_eq!(bases(&asked_now), 0, "{asked_now:?}");
        assert_eq!(listings(&asked_now), 1, "{asked_now:?}");
        assert!(asked_now.iter().any(|r| r.contains("/commits/f1/changes")));
        view.read_with(cx, |view, _| {
            assert!(matches!(view.load, LoadState::Ready));
            assert_eq!(ids(view), ["f2", "f1"]);
            assert_eq!(view.data.as_ref().unwrap().fork.as_deref(), Some("m1"));
        });

        view.update(cx, |view, cx| view.set_scope(Some("f2".into()), cx));
        cx.run_until_parked();
        let asked_now = take();
        assert_eq!(bases(&asked_now), 0, "{asked_now:?}");
        assert_eq!(listings(&asked_now), 0, "the commits held: {asked_now:?}");
        assert!(asked_now.iter().any(|r| r.contains("/commits/f2/changes")));
        view.read_with(cx, |view, _| assert_eq!(ids(view), ["f2", "f1"]));

        view.update(cx, |view, cx| view.set_scope(None, cx));
        cx.run_until_parked();
        let asked_now = take();
        assert_eq!(bases(&asked_now), 0, "{asked_now:?}");
        assert!(
            asked_now
                .iter()
                .any(|r| r.contains("/changes?branch=feature"))
        );
        view.read_with(cx, |view, _| {
            assert_eq!(view.change_count(), 2);
            assert_eq!(ids(view), ["f2", "f1"]);
        });

        // Another writer's commit, seen by the session: the heads held no longer stand.
        session.update(cx, |session, cx| {
            session
                .external_write("feature".into(), "Agent write".into(), false, cx)
                .finish(graph_store::OutcomeKind::Committed {
                    commit: Some("f3".into()),
                })
        });
        cx.run_until_parked();
        take();
        view.update(cx, |view, cx| view.set_scope(Some("f1".into()), cx));
        cx.run_until_parked();
        let asked_now = take();
        assert_eq!(bases(&asked_now), 1, "{asked_now:?}");
    }

    /// A merge deletes the branch unless the person turns that off, and says what became of it.
    #[gpui::test]
    async fn a_merge_deletes_the_branch_unless_the_person_says_not(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        for delete in [true, false] {
            let (session, asked) = review_server(false, None, "f2", |_| (500, json!({})), cx);
            let (view, cx) = cx.add_window_view(|window, cx| {
                ReviewView::new(session, "feature".into(), "main".into(), window, cx)
            });
            cx.run_until_parked();
            view.update(cx, |view, cx| {
                assert!(view.delete_branch, "on unless turned off");
                view.delete_branch = delete;
                view.merge(cx);
            });
            cx.run_until_parked();
            let (previews, merges): (Vec<String>, Vec<String>) = asked
                .lock()
                .unwrap()
                .iter()
                .filter(|request| request.contains("/branches/merge?"))
                .cloned()
                .partition(|request| request.contains("\"dry_run\":true"));
            // The review asked what a merge would do once, writing nothing, before the merge.
            assert_eq!(previews.len(), 1, "{previews:?}");
            assert_eq!(merges.len(), 1, "{merges:?}");
            assert_eq!(merges[0].contains("\"delete_branch\":true"), delete);
            view.read_with(cx, |view, cx| {
                match &view.merge {
                    MergeState::Merged { deleted, .. } => {
                        assert_eq!(deleted.as_ref().map(Result::is_ok), delete.then_some(true));
                    }
                    _ => panic!("not merged"),
                }
                // A session on the deleted branch moves to the branch it was merged into.
                let branch = view.session.read(cx).branch.clone();
                assert_eq!(branch, if delete { "main" } else { "feature" });
                // The merge is the end: Merge is not offered again, and a deleted branch's
                // review is neither watched nor opened again with the window.
                assert!(!view.mergeable(cx));
                assert_eq!(view._head_watch.is_none(), delete);
                assert_eq!(view.serialize(cx).is_none(), delete);
            });
            // Nothing is fixed on a branch the merge deleted.
            if delete {
                view.update(cx, |view, cx| {
                    view.revert(0, cx);
                    assert_eq!(view.cards[0].fix, FixState::Refused(BRANCH_DELETED.into()));
                });
            }
            // A Refresh after the merge does not bring the deleted branch's tab back.
            view.update(cx, |view, cx| view.reload(cx));
            cx.run_until_parked();
            view.read_with(cx, |view, cx| {
                assert!(matches!(view.merge, MergeState::Idle));
                assert_eq!(view.serialize(cx).is_none(), delete);
            });
            // A branch kept, then deleted outside Studio: once the list no longer holds it, its
            // review is the deleted branch's.
            if !delete {
                let session = view.read_with(cx, |view, _| view.session.clone());
                session.update(cx, |session, cx| {
                    session.branches = graph_store::Loadable::Loaded(vec!["main".into()]);
                    cx.emit(graph_store::SessionEvent::BranchesChanged);
                });
                view.update(cx, |view, cx| {
                    assert!(view._head_watch.is_none());
                    assert!(view.serialize(cx).is_none());
                    view.revert(0, cx);
                    assert_eq!(view.cards[0].fix, FixState::Refused(BRANCH_DELETED.into()));
                });
            }
        }
    }

    /// A merge the access route says the credential may not make is not offered: Merge is
    /// disabled and its key not registered, the branch is not deleted with it, and neither the
    /// dry run nor the merge is sent. The review asked about the branch and its target once.
    #[gpui::test]
    async fn a_merge_the_credential_may_not_make_is_not_offered(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        // Changes on every branch, and nothing else of a branch's.
        let access = json!({ "graph_id": "knowledge", "actor_id": "act-dev", "graph_wide": ["read"], "actions": ["read", "change"] });
        let (session, asked) = review_server_with(
            Some(access),
            false,
            None,
            "f2",
            |_| (500, json!({})),
            |_, _, _| None,
            cx,
        );
        let (view, cx) = cx.add_window_view(|window, cx| {
            ReviewView::new(session, "feature".into(), "main".into(), window, cx)
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.focus(&view.focus_handle(cx), cx));
        cx.run_until_parked();
        assert!(!cx.update(|window, cx| window.is_action_available(&Merge, cx)));
        view.update(cx, |view, cx| {
            assert!(view.mergeable(cx), "the whole review is on screen");
            assert_eq!(
                view.merge_refusal(cx).as_deref(),
                Some("Your credential may not merge feature into main")
            );
            assert!(!view.delete_after_merge(cx));
            view.merge(cx);
        });
        cx.run_until_parked();
        let asked = asked.lock().unwrap();
        let reads = |route: &str| asked.iter().filter(|r| r.contains(route)).count();
        assert_eq!(reads("/access?branch=feature&target_branch=main"), 1);
        assert_eq!(reads("/branches/merge?"), 0, "{asked:?}");
    }

    /// A revert or an edit binds one statement to every value it read: the server's answer
    /// that it matched nothing (a value changed since) leaves the card unfixed and unreviewed,
    /// though a commit came back and the head the next fix binds to moved to it.
    #[gpui::test]
    async fn a_fix_that_matched_nothing_is_not_shown_as_done(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let (session, _) = reviewed_server_writing_affecting(false, 0, cx);
        let (view, cx) = cx.add_window_view(|window, cx| {
            ReviewView::new(session, "feature".into(), "main".into(), window, cx)
        });
        cx.run_until_parked();
        view.update(cx, |view, cx| view.revert(0, cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.cards[0].fix, FixState::Unchanged);
            assert!(!view.cards[0].reviewed);
            assert_eq!(view.bound_head.as_deref(), Some("r1"));
        });
    }

    /// A review of one commit does not show what the branch holds now: an addition is not
    /// reverted from it, since its delete is bound to no value and would take what later
    /// commits changed with it. The button says why, and the key sends nothing.
    #[gpui::test]
    async fn a_review_of_one_commit_reverts_no_addition(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let (session, asked) = reviewed_server_writing(false, cx);
        let (view, cx) = cx.add_window_view(|window, cx| {
            ReviewView::new(session, "feature".into(), "main".into(), window, cx)
        });
        cx.run_until_parked();
        view.update(cx, |view, cx| view.set_scope(Some("f1".into()), cx));
        cx.run_until_parked();
        asked.lock().unwrap().clear();
        view.update(cx, |view, cx| {
            assert!(matches!(view.load, LoadState::Ready));
            assert_eq!(view.change_count(), 1);
            assert_eq!(
                view.cards[0].revert_blocked.as_deref(),
                Some(ADDITION_NEEDS_HEAD)
            );
            view.revert(0, cx);
            assert_eq!(view.fixes_running, 0);
        });
        cx.run_until_parked();
        assert!(
            !asked
                .lock()
                .unwrap()
                .iter()
                .any(|request| request.contains("/mutate"))
        );
    }

    /// The dry run is for the head the cards show: a review whose branch moved while it was
    /// read shows none, and neither a target that moves nor fixes that settle read one, until
    /// a Refresh shows the new head.
    #[gpui::test]
    async fn a_review_newer_than_its_cards_shows_no_dry_run(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let (session, asked) = reviewed_server(false, cx);
        let (view, cx) = cx.add_window_view(|window, cx| {
            ReviewView::new(session, "feature".into(), "main".into(), window, cx)
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(
                matches!(view.preview, Preview::Merges(_)),
                "{:?}",
                view.preview
            )
        });
        asked.lock().unwrap().clear();
        view.update(cx, |view, cx| {
            let mut data = view.data.take().unwrap();
            data.moved_while_reading = true;
            view.finish_load(Ok(data), Searched::default(), cx);
            assert!(view.newer);
            assert!(matches!(view.preview, Preview::Unread));
            view.preview_moved(cx);
            view.preview_again = true;
            view.writes_settled(cx);
            assert!(matches!(view.preview, Preview::Unread));
        });
        cx.run_until_parked();
        assert!(
            !asked
                .lock()
                .unwrap()
                .iter()
                .any(|request| request.contains("\"dry_run\":true")),
            "{:?}",
            asked.lock().unwrap()
        );
    }

    /// Draws a review tab, cached, while it is its pane's active item, as a pane draws it.
    struct PaneStandIn {
        review: Entity<ReviewView>,
        shown: bool,
    }

    impl Render for PaneStandIn {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full().when(self.shown, |pane| {
                pane.child(
                    gpui::AnyView::from(self.review.clone())
                        .cached(gpui::StyleRefinement::default().size_full()),
                )
            })
        }
    }

    /// A review behind another tab sends no dry run when its target moves: shown again, it
    /// sends one for all the moves. Before, every open review asked the server to classify its
    /// whole merge each time main moved, unseen.
    #[gpui::test]
    async fn a_hidden_review_sends_no_dry_run_until_shown_again(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let (session, asked) = reviewed_server(false, cx);
        let held = session.clone();
        let (pane, cx) = cx.add_window_view(move |window, cx| {
            let review =
                cx.new(|cx| ReviewView::new(held, "feature".into(), "main".into(), window, cx));
            PaneStandIn {
                review,
                shown: true,
            }
        });
        cx.run_until_parked();
        let dry_runs = |asked: &std::sync::Mutex<Vec<String>>| {
            asked
                .lock()
                .unwrap()
                .iter()
                .filter(|request| request.contains("\"dry_run\":true"))
                .count()
        };
        assert_eq!(dry_runs(&asked), 1, "the review's own dry run");
        pane.update(cx, |pane, cx| {
            pane.shown = false;
            cx.notify();
        });
        cx.run_until_parked();
        for head in ["m8", "m9"] {
            session.update(cx, |_, cx| {
                cx.emit(SessionEvent::HeadMoved {
                    branch: "main".into(),
                    head: head.into(),
                })
            });
            cx.run_until_parked();
        }
        assert_eq!(dry_runs(&asked), 1, "nothing sent while hidden");
        pane.update(cx, |pane, cx| {
            pane.shown = true;
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(dry_runs(&asked), 2, "one for both moves");
    }

    /// A review read while another writer changed the schema is newer once it is in, as is
    /// one read at a head behind the one the session saw, read again with ⌘R; neither sends a
    /// dry run for the head its cards show.
    #[gpui::test]
    async fn a_review_read_behind_the_schema_or_the_head_is_newer(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let (session, asked) = reviewed_server(false, cx);
        let (view, cx) = cx.add_window_view({
            let session = session.clone();
            |window, cx| ReviewView::new(session, "feature".into(), "main".into(), window, cx)
        });
        cx.run_until_parked();
        let dry_runs = |asked: &std::sync::Mutex<Vec<String>>| {
            asked
                .lock()
                .unwrap()
                .iter()
                .filter(|request| request.contains("\"dry_run\":true"))
                .count()
        };
        let assert_newer = |view: &ReviewView| {
            assert!(matches!(view.load, LoadState::Ready));
            assert!(view.newer);
            assert!(
                matches!(view.preview, Preview::Unread),
                "{:?}",
                view.preview
            );
        };

        asked.lock().unwrap().clear();
        view.update(cx, |view, cx| view.reload(cx));
        session.update(cx, |_, cx| cx.emit(SessionEvent::SchemaMoved));
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert_newer(view));
        assert_eq!(dry_runs(&asked), 0);

        // Another writer's commit f3, seen by the session; the review read again shows f2.
        session.update(cx, |session, cx| {
            session
                .external_write("feature".into(), "Agent write".into(), false, cx)
                .finish(graph_store::OutcomeKind::Committed {
                    commit: Some("f3".into()),
                })
        });
        cx.run_until_parked();
        asked.lock().unwrap().clear();
        // ⌘R, where the review has the keyboard, is its Refresh.
        cx.update(|window, cx| window.focus(&view.focus_handle(cx), cx));
        cx.dispatch_action(Refresh);
        cx.run_until_parked();
        assert!(
            !asked.lock().unwrap().is_empty(),
            "the review was read again"
        );
        view.read_with(cx, |view, _| {
            assert_eq!(view.bound_head.as_deref(), Some("f2"));
            assert_newer(view);
        });
        assert_eq!(dry_runs(&asked), 0);
    }

    /// A fix asked for while the merge is out is refused on its card until the merge is back,
    /// and offered again then.
    #[gpui::test]
    async fn a_fix_refused_for_a_running_merge_is_offered_again_once_it_is_back(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let (session, asked) = reviewed_server(false, cx);
        let (view, cx) = cx.add_window_view(|window, cx| {
            ReviewView::new(session, "feature".into(), "main".into(), window, cx)
        });
        cx.run_until_parked();
        view.update(cx, |view, cx| {
            view.merge(cx);
            view.revert(0, cx);
            assert_eq!(view.cards[0].fix, FixState::Refused(MERGE_RUNNING.into()));
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(matches!(view.merge, MergeState::Merged { .. }));
            assert_eq!(view.cards[0].fix, FixState::Idle);
        });
        assert!(
            !asked
                .lock()
                .unwrap()
                .iter()
                .any(|request| request.contains("/mutate"))
        );
    }

    /// A schema that arrives after the review was grouped (one loaded after a tab restored
    /// with the window read, or one naming nodes otherwise in the settings) groups the cards
    /// again from the changes read, without reading the branch again.
    #[gpui::test]
    async fn a_schema_that_arrives_after_the_read_groups_the_cards_again(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let (session, asked) = reviewed_server(false, cx);
        let (view, cx) = cx.add_window_view({
            let session = session.clone();
            |window, cx| ReviewView::new(session, "feature".into(), "main".into(), window, cx)
        });
        cx.run_until_parked();
        let loaded = session
            .read_with(cx, |session, _| session.schema_model())
            .expect("the schema loaded");
        view.update(cx, |view, cx| {
            // Read while the schema had not loaded: grouped without it.
            let mut data = view.data.take().unwrap();
            data.schema = None;
            data.groups = EntityGroups::new(&data.folded, None);
            view.finish_load(Ok(data), Searched::default(), cx);
        });
        asked.lock().unwrap().clear();
        cx.run_until_parked();
        let grouped_with = |view: &ReviewView| view.data.as_ref().unwrap().schema.clone();
        view.read_with(cx, |view, _| {
            assert!(same_schema(grouped_with(view).as_ref(), Some(&loaded)));
            let data = view.data.as_ref().unwrap();
            assert_eq!(data.groups, EntityGroups::new(&data.folded, Some(&*loaded)));
        });
        // A schema read again with the same text, as after the graph answers again, is the same
        // one: the cards stay as grouped, where the person scrolled to.
        let grouped = view.read_with(cx, |view, _| grouped_with(view).unwrap());
        session.update(cx, |session, cx| session.reload_schema(cx));
        cx.run_until_parked();
        let again = session
            .read_with(cx, |session, _| session.schema_model())
            .unwrap();
        assert!(
            Arc::ptr_eq(&again, &loaded),
            "the schema held, not compiled again"
        );
        view.read_with(cx, |view, _| {
            assert!(same_schema(grouped_with(view).as_ref(), Some(&again)));
            assert!(
                Arc::ptr_eq(&grouped_with(view).unwrap(), &grouped),
                "not grouped again"
            );
            assert!(view._regroup_task.is_none());
        });
        let asked = asked.lock().unwrap();
        assert!(
            !asked
                .iter()
                .any(|request| request.contains("/changes") || request.contains("/merge-base")),
            "read again: {asked:?}"
        );
    }

    /// A server for a review of `feature` against `main`: `feature` forked at m1 and committed
    /// f1 (a person added) and f2 (one changed). Every request's path and query is recorded.
    /// With `merged_in`, `main`'s m2 was merged into `feature`: the merge base is m2, off
    /// `feature`'s first-parent history, and the feed refuses it as a start.
    pub(crate) fn reviewed_server(
        merged_in: bool,
        cx: &mut gpui::TestAppContext,
    ) -> (
        Entity<GraphSession>,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        review_server(merged_in, None, "f2", |_| (500, json!({})), cx)
    }

    /// As [`reviewed_server`], the branch's writes committing r1, r2, … each on the last.
    fn reviewed_server_writing(
        merged_in: bool,
        cx: &mut gpui::TestAppContext,
    ) -> (
        Entity<GraphSession>,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        reviewed_server_writing_affecting(merged_in, 1, cx)
    }

    /// As [`reviewed_server_writing`], each write saying it changed `affected` nodes.
    fn reviewed_server_writing_affecting(
        merged_in: bool,
        affected: i64,
        cx: &mut gpui::TestAppContext,
    ) -> (
        Entity<GraphSession>,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        // Two people added: each revert deletes one.
        let added = |id: &str| json!({ "kind": "node", "type": { "id": "Person", "name": "Person" }, "id": id, "op": "insert", "before": null, "after": { "properties": { "slug": id, "name": "x" } } });
        let block = |id: &str, parent: &str, change: Value| json!({ "cause": { "graph_commit_id": id, "parent_commit_id": parent, "authored_branch": "feature", "authored_at": 0, "actor_id": "act-dev" }, "changes": [change] });
        let feed = json!({ "blocks": [block("f1", "m1", added("p1")), block("f2", "f1", added("p2"))], "cursor": "c", "caught_up": true });
        review_server(
            merged_in,
            Some(feed),
            "f2",
            move |n| {
                let parent = if n == 0 {
                    "f2".to_owned()
                } else {
                    format!("r{n}")
                };
                (
                    200,
                    json!({ "branch": "feature", "query_name": "q", "affected_nodes": affected, "affected_edges": 0,
                        "commit": { "graph_commit_id": format!("r{}", n + 1), "graph_manifest_version": 3, "kind": "published", "created_at": 0, "parent_commit_id": parent } }),
                )
            },
            cx,
        )
    }

    /// A merge commits m3, merging `took` of `feature`.
    fn review_server(
        merged_in: bool,
        feed: Option<Value>,
        took: &'static str,
        write: impl Fn(usize) -> (u16, Value) + Send + Sync + 'static,
        cx: &mut gpui::TestAppContext,
    ) -> (
        Entity<GraphSession>,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        review_server_with(None, merged_in, feed, took, write, |_, _, _| None, cx)
    }

    /// As [`review_server`], the access route answering `access` for every branch; without it,
    /// it answers nothing Studio can read, so what the credential may do stays unknown. `over`
    /// answers first, for each request's path, query and body, where it has an answer.
    fn review_server_with(
        access: Option<Value>,
        merged_in: bool,
        feed: Option<Value>,
        took: &'static str,
        write: impl Fn(usize) -> (u16, Value) + Send + Sync + 'static,
        over: impl Fn(&str, &str, &str) -> Option<(u16, Value)> + Send + Sync + 'static,
        cx: &mut gpui::TestAppContext,
    ) -> (
        Entity<GraphSession>,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        let writes = std::sync::atomic::AtomicUsize::new(0);
        let commit = |id: &str, parent: Option<&str>| json!({ "graph_commit_id": id, "graph_manifest_version": 1, "kind": "published", "created_at": 0, "parent_commit_id": parent, "actor_id": "act-dev" });
        let person = |id: &str, op: &str| json!({ "kind": "node", "type": { "id": "Person", "name": "Person" }, "id": id, "op": op, "before": { "properties": { "slug": id } }, "after": { "properties": { "slug": id, "name": "x" } } });
        let block = |id: &str, parent: &str, change: Value| json!({ "cause": { "graph_commit_id": id, "parent_commit_id": parent, "authored_branch": "feature", "authored_at": 0, "actor_id": "act-dev" }, "changes": [change] });
        let base_id = if merged_in { "m2" } else { "m1" };
        let base = json!({ "merge_base": commit(base_id, Some("m1")), "source": "feature", "source_commit_id": "f2", "target": "main", "target_commit_id": "m2" });
        // Merged in, main's m2 came into the branch by the merge commit fm, whose feed block
        // carries main's own change (p9); the net diff from the merge base does not.
        let feed = feed.unwrap_or_else(|| {
            let mut blocks = vec![block("f1", "m1", person("p1", "insert"))];
            if merged_in {
                blocks.push(block("fm", "f1", person("p9", "insert")));
            }
            blocks.push(block(
                "f2",
                if merged_in { "fm" } else { "f1" },
                person("p2", "update"),
            ));
            json!({ "blocks": blocks, "cursor": "c", "caught_up": true })
        });
        let diff = json!({ "from_commit_id": "m2", "to_commit_id": "f2", "changes": [person("p1", "insert"), person("p2", "update")] });
        let feature = if merged_in {
            let mut merge = commit("fm", Some("f1"));
            merge["merged_parent_commit_id"] = json!("m2");
            json!({ "commits": [commit("f2", Some("fm")), merge, commit("f1", Some("m1")), commit("m1", None)] })
        } else {
            json!({ "commits": [commit("f2", Some("f1")), commit("f1", Some("m1")), commit("m1", None)] })
        };
        let main = json!({ "commits": [commit("m2", Some("m1")), commit("m1", None)] });
        fake_server(
            move |path, query, sent| {
                if let Some(answer) = over(path, query, sent) {
                    answer
                } else if let Some(access) = access.as_ref().filter(|_| path.ends_with("/access")) {
                    (200, access.clone())
                } else if path.contains("/mutate") {
                    write(writes.fetch_add(1, std::sync::atomic::Ordering::SeqCst))
                } else if path.ends_with("/branches/merge") {
                    let merge = json!({ "graph_commit_id": "m3", "graph_manifest_version": 4, "kind": "published", "created_at": 0, "parent_commit_id": "m2", "merged_parent_commit_id": took });
                    let mut answer = json!({ "source": "feature", "target": "main", "outcome": "merged", "commit": merge });
                    if sent.contains("\"delete_branch\":true") {
                        answer["branch_deleted"] = json!(true);
                    }
                    (200, answer)
                } else if path.ends_with("/schema") {
                    (
                        200,
                        json!({ "schema_source": "node Person {\n  slug: String @key\n  name: String?\n}\n" }),
                    )
                } else if path.ends_with("/branches/merge-base") {
                    (200, base.clone())
                } else if let Some((_, id)) = path
                    .strip_suffix("/changes")
                    .and_then(|commit| commit.rsplit_once("/commits/"))
                {
                    // One commit's own changes: its block of the feed.
                    let blocks = feed["blocks"].as_array().cloned().unwrap_or_default();
                    match blocks
                        .iter()
                        .find(|block| block["cause"]["graph_commit_id"] == id)
                    {
                        Some(block) => (
                            200,
                            json!({ "cause": block["cause"], "changes": block["changes"] }),
                        ),
                        None => (
                            404,
                            json!({ "error": "no such commit", "code": "not_found" }),
                        ),
                    }
                } else if path.ends_with("/changes") && query.contains("start=after%3Am2") {
                    (
                        400,
                        json!({ "error": "start commit is not on this branch's first-parent chain", "code": "bad_request", "disposition": { "action": "fix", "effects": "none" } }),
                    )
                } else if path.ends_with("/changes") {
                    (200, feed.clone())
                } else if path.ends_with("/diff") && query.contains("from=m2&to=f2") {
                    (200, diff.clone())
                } else if path.ends_with("/commits") && query.contains("branch=feature") {
                    (200, feature.clone())
                } else if path.ends_with("/commits") {
                    (200, main.clone())
                } else {
                    (200, json!({ "branches": ["main", "feature"] }))
                }
            },
            cx,
        )
    }

    /// A session on a server answering each request's path, query and body with `answer`;
    /// every request is recorded, a merge with its body.
    pub(crate) fn fake_server(
        answer: impl Fn(&str, &str, &str) -> (u16, Value) + Send + Sync + 'static,
        cx: &mut gpui::TestAppContext,
    ) -> (
        Entity<GraphSession>,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        use gpui::AppContext as _;
        use graph_store::{Connection, ServerEntry, ServerSource};
        use http_client::http::Response;
        use http_client::{AsyncBody, FakeHttpClient};

        let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = asked.clone();
        let answer = std::sync::Arc::new(answer);
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            let query = request.uri().query().unwrap_or_default().to_owned();
            let bound = request
                .headers()
                .get(graph_client::IF_GRAPH_COMMIT_HEADER)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_owned();
            let (seen, answer) = (seen.clone(), answer.clone());
            async move {
                use futures::AsyncReadExt as _;
                let mut sent = String::new();
                request.into_body().read_to_string(&mut sent).await.ok();
                let merge = if path.ends_with("/branches/merge") {
                    format!(" {sent}")
                } else {
                    String::new()
                };
                seen.lock()
                    .unwrap()
                    .push(format!("{path}?{query} {bound}{merge}"));
                let (status, body) = answer(&path, &query, &sent);
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
            cx.new(|cx| GraphSession::new(connection, "knowledge".into(), "feature".into(), cx));
        cx.run_until_parked();
        (session, asked)
    }

    /// A review reads the merge base and the branch's feed after it, never the two commit
    /// lists; a review of one commit reads the branch's history a page at a time down to the
    /// merge base, and never main's. A branch main was merged into has a feed that refuses the
    /// base as a start, and whose merge block carries main's own changes: the review reads the
    /// net diff from the base instead, and a page of each history for the branch's commits.
    /// No history is ever asked for whole.
    #[gpui::test]
    async fn a_review_reads_the_merge_base_and_the_feed_after_it(cx: &mut gpui::TestAppContext) {
        let (session, asked) = reviewed_server(false, cx);
        asked.lock().unwrap().clear();
        let mut async_cx = cx.to_async();
        let data = read_review(
            session,
            "feature".into(),
            "main".into(),
            None,
            None,
            WeakEntity::new_invalid(),
            &mut async_cx,
        )
        .await
        .unwrap();
        assert_eq!(data.fork.as_deref(), Some("m1"));
        assert_eq!(data.head.as_deref(), Some("f2"));
        let commits: Vec<&str> = data.commits.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(commits, ["f2", "f1"], "newest first");
        assert_eq!(data.folded.changes.len(), 2);
        assert!(!data.partial && !data.moved_while_reading);
        let asked = asked.lock().unwrap().clone();
        assert!(
            asked.iter().all(|request| !request.contains("/commits")),
            "no commit list: {asked:?}"
        );
        assert!(
            asked
                .iter()
                .any(|request| request.contains("start=after%3Am1"))
        );

        let (session, asked) = reviewed_server(false, cx);
        asked.lock().unwrap().clear();
        let mut async_cx = cx.to_async();
        let data = read_review(
            session,
            "feature".into(),
            "main".into(),
            Some("f1".into()),
            None,
            WeakEntity::new_invalid(),
            &mut async_cx,
        )
        .await
        .unwrap();
        assert_eq!(data.fork.as_deref(), Some("m1"));
        let commits: Vec<&str> = data.commits.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(commits, ["f2", "f1"], "newest first");
        let asked = asked.lock().unwrap().clone();
        let lists: Vec<&String> = asked.iter().filter(|r| r.contains("/commits?")).collect();
        assert_eq!(lists.len(), 1, "{asked:?}");
        assert!(lists[0].contains("branch=feature&limit=500"), "{asked:?}");

        let (session, asked) = reviewed_server(true, cx);
        asked.lock().unwrap().clear();
        let mut async_cx = cx.to_async();
        let data = read_review(
            session,
            "feature".into(),
            "main".into(),
            None,
            None,
            WeakEntity::new_invalid(),
            &mut async_cx,
        )
        .await
        .unwrap();
        assert_eq!(data.fork.as_deref(), Some("m2"), "the merge base");
        assert_eq!(data.head.as_deref(), Some("f2"));
        assert!(data.merged_in);
        let ids: Vec<&str> = data.folded.changes.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["p1", "p2"], "the branch's own changes, not main's p9");
        let commits: Vec<&str> = data.commits.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(commits, ["f2", "fm", "f1"]);
        let asked = asked.lock().unwrap().clone();
        assert!(
            asked
                .iter()
                .any(|request| request.contains("/diff?from=m2&to=f2")),
            "{asked:?}"
        );
        // A page of each, asked for together, beside the diff, which does not wait for them.
        let lists: Vec<&String> = asked.iter().filter(|r| r.contains("/commits?")).collect();
        assert_eq!(lists.len(), 2, "a page of each: {asked:?}");
        for branch in ["feature", "main"] {
            let page = format!("branch={branch}&limit=500");
            assert!(lists.iter().any(|list| list.contains(&page)), "{asked:?}");
        }
        assert_eq!(data.commits_capped, None);
    }

    /// A commit as a listing has it: `id`, its parent and merged parent, and the branch that
    /// published it as the server records it: `feature` for an `f…` id, `dev` for a `d…` one,
    /// main (none) otherwise.
    fn listed(id: &str, parent: Option<&str>, merged: Option<&str>) -> Value {
        let branch = match id.chars().next() {
            Some('f') => Some("feature"),
            Some('d') => Some("dev"),
            _ => None,
        };
        json!({ "graph_commit_id": id, "graph_branch": branch, "graph_manifest_version": 1, "kind": "published", "created_at": 0, "parent_commit_id": parent, "merged_parent_commit_id": merged, "actor_id": "act-dev" })
    }

    /// A chain of `n` commits, newest first, `{prefix}{n-1}` down to `{prefix}0`, whose oldest
    /// commit's parent is `below`.
    fn chain_of(prefix: &str, n: usize, below: Option<&str>) -> Vec<Value> {
        (0..n)
            .rev()
            .map(|i| {
                let parent = match i {
                    0 => below.map(str::to_owned),
                    i => Some(format!("{prefix}{}", i - 1)),
                };
                listed(&format!("{prefix}{i}"), parent.as_deref(), None)
            })
            .collect()
    }

    /// A page of `chain` as Omnigraph pages a listing: `limit` commits from the place a
    /// `page_token` names, and a token for the next page while more follow. Without `limit`,
    /// the whole chain.
    fn page_of(chain: &[Value], query: &str) -> Value {
        let pairs: HashMap<&str, &str> = query
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .collect();
        let Some(limit) = pairs
            .get("limit")
            .and_then(|limit| limit.parse::<usize>().ok())
        else {
            return json!({ "commits": chain });
        };
        let start = pairs
            .get("page_token")
            .and_then(|token| token.parse::<usize>().ok())
            .unwrap_or(0)
            .min(chain.len());
        let end = (start + limit).min(chain.len());
        let next = (end < chain.len()).then(|| end.to_string());
        json!({ "commits": &chain[start..end], "head_commit_id": chain.first().map(|c| c["graph_commit_id"].clone()), "next_page_token": next })
    }

    /// A server for `feature` and `main` whose histories page, with `base` their merge base and
    /// `feature`'s first commit its head. `feature`'s feed refuses the base as a start when
    /// main was merged into it (`merged_in`); every diff and commit holds one person's change.
    fn paged_server(
        feature: Vec<Value>,
        main: Vec<Value>,
        base: &str,
        merged_in: bool,
        cx: &mut gpui::TestAppContext,
    ) -> (
        Entity<GraphSession>,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        let head = feature[0]["graph_commit_id"].as_str().unwrap().to_owned();
        paged_server_at(feature, main, base, &head, merged_in, cx)
    }

    /// [`paged_server`] whose merge base names `head` as `feature`'s head: the branch moved on
    /// past it before its listing was read, which starts at the commits made since.
    fn paged_server_at(
        feature: Vec<Value>,
        main: Vec<Value>,
        base: &str,
        head: &str,
        merged_in: bool,
        cx: &mut gpui::TestAppContext,
    ) -> (
        Entity<GraphSession>,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        let merge_base = json!({ "merge_base": listed(base, None, None), "source": "feature", "source_commit_id": head, "target": "main", "target_commit_id": main[0]["graph_commit_id"] });
        let person = json!({ "kind": "node", "type": { "id": "Person", "name": "Person" }, "id": "p1", "op": "insert", "before": null, "after": { "properties": { "slug": "p1" } } });
        let refused = format!("start=after%3A{base}");
        fake_server(
            move |path, query, _| {
                if path.ends_with("/branches/merge-base") {
                    (200, merge_base.clone())
                } else if path.ends_with("/schema") {
                    (
                        200,
                        json!({ "schema_source": "node Person {\n  slug: String @key\n}\n" }),
                    )
                } else if path.ends_with("/commits") && query.contains("branch=feature") {
                    (200, page_of(&feature, query))
                } else if path.ends_with("/commits") {
                    (200, page_of(&main, query))
                } else if let Some((_, id)) = path
                    .strip_suffix("/changes")
                    .and_then(|commit| commit.rsplit_once("/commits/"))
                {
                    let cause = json!({ "graph_commit_id": id, "authored_branch": "feature", "authored_at": 0, "actor_id": "act-dev" });
                    (200, json!({ "cause": cause, "changes": [person] }))
                } else if path.ends_with("/changes") && merged_in && query.contains(&refused) {
                    (
                        400,
                        json!({ "error": "start commit is not on this branch's first-parent chain", "code": "bad_request", "disposition": { "action": "fix", "effects": "none" } }),
                    )
                } else if path.ends_with("/diff") {
                    (
                        200,
                        json!({ "from_commit_id": "x", "to_commit_id": "y", "changes": [person] }),
                    )
                } else {
                    (200, json!({ "branches": ["main", "feature"] }))
                }
            },
            cx,
        )
    }

    /// Listings asked for, and none whole: each names a page size.
    fn listings(asked: &std::sync::Arc<std::sync::Mutex<Vec<String>>>, branch: &str) -> usize {
        let asked = asked.lock().unwrap();
        let lists: Vec<&String> = asked.iter().filter(|r| r.contains("/commits?")).collect();
        assert!(
            lists.iter().all(|list| list.contains("limit=")),
            "a history asked for whole: {lists:?}"
        );
        lists
            .iter()
            .filter(|list| list.contains(&format!("branch={branch}&")))
            .count()
    }

    /// A fork past the history a review reads is not looked for in the whole lists: the review
    /// shows the branch's own commits it read, says which history it stopped reading, and
    /// names no commit count. Past the branch's own pages (a review of one commit on a long
    /// branch), the merge base the server names is the fork, and the count is of the commits at
    /// or below the reviewed head, not of those the branch made after it; past main's (a branch
    /// main was merged into, whose fork is deep in main), and past the branch's pages below its
    /// own commits (a branch forked from another), the branch's own commits are shown, and
    /// none of the history it shares with another branch.
    #[gpui::test]
    async fn a_fork_past_the_history_read_is_said_and_no_history_is_read_whole(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        // More of feature's own commits than a review reads: main is not read at all.
        let feature = chain_of("f", FORK_BOUND + 500, Some("m0"));
        let (session, asked) = paged_server(feature, chain_of("m", 1, None), "m0", false, cx);
        let mut async_cx = cx.to_async();
        let data = read_review(
            session,
            "feature".into(),
            "main".into(),
            Some("f10499".into()),
            None,
            WeakEntity::new_invalid(),
            &mut async_cx,
        )
        .await
        .unwrap();
        assert_eq!(data.commits.len(), FORK_BOUND, "the commits read");
        assert_eq!(data.commits[0].id, "f10499", "newest first");
        assert_eq!(data.commits_capped, Some(Capped::Own { read: FORK_BOUND }));
        assert!(!data.merged_in);
        assert_eq!(data.fork.as_deref(), Some("m0"), "the merge base");
        assert_eq!(
            capped_commits_note(&data, "feature", "main").as_deref(),
            Some(
                "More than 10,000 commits since the fork: Studio read no further, and One \
                 commit lists only those it read."
            )
        );
        assert_eq!(listings(&asked, "feature"), FORK_PAGES);
        assert_eq!(listings(&asked, "main"), 0);

        // feature moved 300 commits past the reviewed head, f9899, before its listing was read:
        // those 300 are read and skipped, so 9,700 of the pages' commits are the review's, and
        // the branch has 9,900 since the fork, not more than 10,000.
        let feature = chain_of("f", FORK_BOUND + 200, Some("m0"));
        let (session, _) =
            paged_server_at(feature, chain_of("m", 1, None), "m0", "f9899", false, cx);
        let mut async_cx = cx.to_async();
        let data = read_review(
            session,
            "feature".into(),
            "main".into(),
            Some("f9899".into()),
            None,
            WeakEntity::new_invalid(),
            &mut async_cx,
        )
        .await
        .unwrap();
        assert_eq!(data.commits.len(), 9_700);
        assert_eq!(data.commits[0].id, "f9899", "nothing made after the head");
        assert_eq!(
            capped_commits_note(&data, "feature", "main").as_deref(),
            Some(
                "More than 9,700 commits since the fork: Studio read no further, and One \
                 commit lists only those it read."
            )
        );

        // feature forked from dev, which has more commits than a review reads above main's m0:
        // feature's two are listed, not dev's, and the note says feature's history stopped the
        // read below them.
        let mut feature = vec![
            listed("f1", Some("f0"), None),
            listed("f0", Some("d9999"), None),
        ];
        feature.extend(chain_of("d", FORK_BOUND, Some("m0")));
        let (session, asked) = paged_server(feature, chain_of("m", 1, None), "m0", false, cx);
        let mut async_cx = cx.to_async();
        let data = read_review(
            session,
            "feature".into(),
            "main".into(),
            Some("f1".into()),
            None,
            WeakEntity::new_invalid(),
            &mut async_cx,
        )
        .await
        .unwrap();
        let commits: Vec<&str> = data.commits.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(commits, ["f1", "f0"], "dev's commits are not feature's");
        assert_eq!(
            data.commits_capped,
            Some(Capped::Fork {
                made: 2,
                target: false
            })
        );
        assert_eq!(
            capped_commits_note(&data, "feature", "main").as_deref(),
            Some(
                "The fork is more than 10,000 commits down feature's history: Studio read no \
                 further, and One commit lists the 2 commits made on feature."
            )
        );
        assert_eq!(listings(&asked, "feature"), FORK_PAGES);
        assert_eq!(listings(&asked, "main"), 0);

        // main merged into feature at m10500, feature forked at m0: main's pages end before m0.
        let feature = vec![
            listed("f2", Some("fm"), None),
            listed("fm", Some("f1"), Some("m10500")),
            listed("f1", Some("m0"), None),
            listed("m0", None, None),
        ];
        let main = chain_of("m", 10_501, None);
        let (session, asked) = paged_server(feature, main, "m10500", true, cx);
        let (view, cx) = cx.add_window_view(|window, cx| {
            ReviewView::new(session, "feature".into(), "main".into(), window, cx)
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            let data = view.data.as_ref().expect("read");
            assert!(data.merged_in);
            assert_eq!(
                data.commits_capped,
                Some(Capped::Fork {
                    made: 3,
                    target: true
                })
            );
            assert_eq!(data.fork.as_deref(), Some("m10500"));
            let commits: Vec<&str> = data.commits.iter().map(|c| c.id.as_str()).collect();
            assert_eq!(commits, ["f2", "fm", "f1"], "main's m0 is not feature's");
            assert_eq!(
                capped_commits_note(data, "feature", "main").as_deref(),
                Some(
                    "The fork is past the newest 10,000 commits of main's history: Studio read \
                     no further, and One commit lists the 3 commits made on feature."
                )
            );
            assert_eq!(view.summary, "1 entity changed", "no count, nobody");
        });
        assert_eq!(listings(&asked, "feature"), 1);
        assert_eq!(listings(&asked, "main"), FORK_PAGES);
    }

    /// A merge is confirmed by its source head being the merge base; when more of the source
    /// was merged since, the merge base has moved past it, and the target's history shows it.
    /// A source merged whole holds every head it had; one that moved on since does not say.
    #[gpui::test]
    async fn a_merge_is_confirmed_after_more_of_the_source_was_merged(
        cx: &mut gpui::TestAppContext,
    ) {
        let commit = |id: &str, merged: Option<&str>| json!({ "graph_commit_id": id, "graph_manifest_version": 1, "kind": "published", "created_at": 0, "merged_parent_commit_id": merged });
        // main merged f2 (as m3), then f3 (as m4).
        let base = json!({ "merge_base": commit("f3", None), "source": "feature", "source_commit_id": "f3", "target": "main", "target_commit_id": "m4" });
        let main = json!({ "commits": [commit("m4", Some("f3")), commit("m3", Some("f2")), commit("m1", None)] });
        let (session, _) = fake_server(
            move |path, _, _| {
                if path.ends_with("/branches/merge-base") {
                    (200, base.clone())
                } else if path.ends_with("/commits") {
                    (200, main.clone())
                } else {
                    (200, json!({ "branches": ["main", "feature"] }))
                }
            },
            cx,
        );
        let mut async_cx = cx.to_async();
        assert_eq!(
            merge_shown(
                &session,
                "feature",
                "main",
                Some("m3"),
                Some("f2"),
                None,
                &mut async_cx
            )
            .await,
            Check::Shown
        );
        assert_eq!(
            merge_landed(&session, "feature", "main", &sent("f2"), &mut async_cx).await,
            Check::Shown,
            "f2 was merged, though the merge base is f3 now"
        );
        assert_eq!(
            merge_landed(&session, "feature", "main", &sent("f9"), &mut async_cx).await,
            Check::Shown,
            "the source is merged whole, and its heads with it"
        );

        // The source moved on to f4 since main merged f3: a head main's history does not
        // show was not merged.
        let moved = json!({ "merge_base": commit("f3", None), "source": "feature", "source_commit_id": "f4", "target": "main", "target_commit_id": "m4" });
        let main = json!({ "commits": [commit("m4", Some("f3")), commit("m3", Some("f2")), commit("m1", None)] });
        let (session, _) = fake_server(
            move |path, _, _| {
                if path.ends_with("/branches/merge-base") {
                    (200, moved.clone())
                } else if path.ends_with("/commits") {
                    (200, main.clone())
                } else {
                    (200, json!({ "branches": ["main", "feature"] }))
                }
            },
            cx,
        );
        let mut async_cx = cx.to_async();
        assert_eq!(
            merge_landed(&session, "feature", "main", &sent("f9"), &mut async_cx).await,
            Check::NotShown
        );

        // A history that cannot be read tells nothing: not "not found".
        let (session, _) = fake_server(|_, _, _| (500, json!({ "error": "down" })), cx);
        let mut async_cx = cx.to_async();
        let shown = merge_shown(
            &session,
            "feature",
            "main",
            Some("m3"),
            Some("f2"),
            None,
            &mut async_cx,
        )
        .await;
        assert_eq!(shown, Check::Unread);
    }

    /// A merge not in the newest pages of the target's history read is not confirmed, and the
    /// rest of the history is not read for it: a committed merge says so, and a merge whose
    /// response was lost stays unknown, never "merge again".
    #[gpui::test]
    async fn a_merge_past_the_history_read_is_not_confirmed(cx: &mut gpui::TestAppContext) {
        // The source moved on to f4 since main merged f3; f2's merge is older than main's
        // newest commits read.
        let base = json!({ "merge_base": listed("f3", None, None), "source": "feature", "source_commit_id": "f4", "target": "main", "target_commit_id": "m10500" });
        let main = chain_of("m", FORK_BOUND + 501, None);
        let (session, asked) = fake_server(
            move |path, query, _| {
                if path.ends_with("/branches/merge-base") {
                    (200, base.clone())
                } else if path.ends_with("/commits") {
                    (200, page_of(&main, query))
                } else {
                    (200, json!({ "branches": ["main", "feature"] }))
                }
            },
            cx,
        );
        let mut async_cx = cx.to_async();
        let landed = merge_landed(&session, "feature", "main", &sent("f2"), &mut async_cx).await;
        assert_eq!(landed, Check::NotReached);
        assert_eq!(listings(&asked, "main"), FORK_PAGES);
        let lost = unknown_merge_words("the connection closed", landed, "main");
        assert_eq!(
            lost,
            "The merge outcome is unknown (the connection closed): the newest 10,000 commits of \
             main's history do not show it, and Studio read no further. Studio did not retry, \
             and Merge stays off until main's history settles it: open History to check, or \
             Refresh to read it again."
        );
        assert_eq!(
            merged_check_words(landed, "main"),
            " Not confirmed: the newest 10,000 commits of main's history do not show it, and \
             Studio read no further; open History to check."
        );
    }
}
