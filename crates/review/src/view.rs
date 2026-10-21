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
    ChangeRequest, CommitOutput, MergeConflictOutput,
};
use graph_client::{ReadError, WriteOutcome};
use graph_store::fixes::revert_deletes;
use graph_store::{
    AccessAction, AccessScope, ChangeFold, EntityGroup, EntityGroups, FoldedChange, FoldedChanges,
    GraphSession, GroupRun, GroupSection, HeadWatch, NodeNames, REVIEW_CHANGE_CAP, SessionEvent,
    TypeCounts, commits_since, contains_merge_of, fork_point, revert_query, summary_sentence,
};
use serde_json::Value;
use studio_actions::review::{
    AcceptAndNext, EditChange, Merge, NextChange, PreviousChange, RevertAndNext,
};
use studio_ui::ds::{radii, space};
use studio_ui::menu::ContextMenu;
use studio_ui::{ChangeCard, change_counts, group_digits, short_commit_id, type_swatch};
use studio_workspace::{Item, ItemEvent, SavedItem, Workspace, workspace_for};
use ui::prelude::*;
use ui::{KeyBinding, PopoverMenu, Severity, Tooltip};

use crate::conflicts::{conflict_wording, resolution_hint};

/// How long typing in the search pauses before the cards are filtered.
const SEARCH_DELAY: Duration = Duration::from_millis(80);

/// Changes asked for per change-feed page: the server's most (`COMMIT_CHANGES_MAX_CHANGES`),
/// as the CLI asks when it follows every page. Each page restarts the server's comparison
/// after the last change returned, so fewer, larger pages cost fewer reads; the server's byte
/// budget still bounds what one page holds.
const FEED_PAGE: usize = 8_192;
/// Commits asked for per page of a branch's history when its commits since the fork are read
/// ([`commits_since_fork`]); a page costs only the commits it returns. The server's most is
/// 1,000.
const FORK_PAGE: u32 = 500;
/// Pages of a branch's history read for its fork before both whole lists are read instead.
const FORK_PAGES: usize = 20;

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
    NotShown,
    /// The history could not be read, or the source head was not known: Studio cannot tell.
    Unread,
}

impl Check {
    fn of(shown: Option<bool>) -> Self {
        match shown {
            Some(true) => Check::Shown,
            Some(false) => Check::NotShown,
            None => Check::Unread,
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
    /// The branch's commits since the fork, newest first.
    commits: Vec<ReviewCommit>,
    /// Shared with the work done off the main thread: grouping again under another schema.
    folded: Arc<FoldedChanges>,
    /// The changes by entity: each node with the relationship changes reviewed on its card.
    groups: EntityGroups,
    /// Each change's search text, lowercase, made when it was folded: a search goes through
    /// these off the main thread, not through every value of every change on each key.
    search: Arc<[String]>,
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

/// Why a fix is not sent while a merge is out.
const MERGE_RUNNING: &str = "A merge is running: a fix now could land on either side of it.";
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
struct ReviewCommit {
    id: String,
    actor_id: Option<String>,
    /// Unix microseconds.
    created_at: i64,
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
                // The dry run merged into the target as it was.
                SessionEvent::HeadMoved { branch, head }
                    if *branch == this.target
                        && this.preview_target.as_deref() != Some(head.as_str()) =>
                {
                    this.preview_moved(cx)
                }
                SessionEvent::SchemaChanged => this.schema_changed(cx),
                SessionEvent::SchemaMoved => this.schema_moved(cx),
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
        view.reload(cx);
        view
    }

    /// The session's schema was loaded, applied, or names nodes otherwise: the review shown is
    /// grouped again under it. One being read compares its schema once it is in.
    fn schema_changed(&mut self, cx: &mut Context<Self>) {
        if matches!(self.load, LoadState::Ready) {
            self.regroup(cx);
        }
        cx.notify();
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

    /// Reads the review again from the branch head. Reviewed marks are kept.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
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
        self.load = LoadState::Loading { folded: 0 };
        self.newer = false;
        self.schema_moved_while_loading = false;
        self.show_conflicts = false;
        if !matches!(self.merge, MergeState::Merging | MergeState::Checking) {
            self.merge = MergeState::Idle;
        }
        let session = self.session.clone();
        let branch = self.branch.clone();
        let target = self.target.clone();
        let scope = self.scope.clone();
        self._load_task = Some(cx.spawn(async move |this, cx| {
            let result = read_review(session, branch, target, scope, this.clone(), cx).await;
            // The search's matches among the changes read, found off the main thread too.
            let searched = match &result {
                Ok(data) => {
                    let query = this
                        .update(cx, |this, cx| this.search_text(cx))
                        .unwrap_or_default();
                    let (texts, text) = (data.search.clone(), query.clone());
                    let matched = cx
                        .background_executor()
                        .spawn(async move { search_matches(&texts, &text) })
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
                .spawn(async move { search_matches(&scanned, &text) })
                .await;
            this.update(cx, |this, cx| {
                if this
                    .data
                    .as_ref()
                    .is_some_and(|data| Arc::ptr_eq(&data.search, &texts))
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

    /// Reviews everything since the fork (`None`) or one commit, reading again.
    fn set_scope(&mut self, scope: Option<String>, cx: &mut Context<Self>) {
        if self.scope != scope {
            self.scope = scope;
            self.reload(cx);
        }
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
        if self.cards.get(ix)?.fix.done() {
            return None;
        }
        if self.merging() || self.branch_deleted {
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

    fn merging(&self) -> bool {
        matches!(self.merge, MergeState::Checking | MergeState::Merging)
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
        // As the button is: a card fixed already shows values its fix replaced, and an edit
        // during a merge could land on either side of it.
        if self.merging() || self.cards.get(ix).is_none_or(|card| card.fix.done()) {
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
        if self.merging() {
            return Err(MERGE_RUNNING.into());
        }
        if self.branch_deleted {
            return Err(BRANCH_DELETED.into());
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
        // Found by the change's key: a Refresh while the fix was out may have moved its card.
        if let Some(card) = self.card_of(key).and_then(|ix| self.cards.get_mut(ix)) {
            if matches!(fix, FixState::Reverted { .. }) {
                card.reviewed = true;
            }
            card.fix = fix;
        }
        if !landed {
            self.drop_fixes_after(key);
        }
        self.send_next_fix(cx);
        self.writes_settled(cx);
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    /// Drops the queued fixes that wait for the fix with `key`, which did not land: the old
    /// relationship of a move is not restored with the new one still there.
    fn drop_fixes_after(&mut self, key: &str) {
        let (dropped, kept): (Vec<Fix>, Vec<Fix>) = std::mem::take(&mut self.fix_queue)
            .into_iter()
            .partition(|fix| fix.after.as_deref() == Some(key));
        self.fix_queue = kept.into();
        for fix in dropped {
            self.fixes_running = self.fixes_running.saturating_sub(1);
            self.fixes_in_flight.remove(&fix.key);
            if let Some(card) = self.card_of(&fix.key).and_then(|ix| self.cards.get_mut(ix)) {
                card.fix = FixState::Refused(
                    "Not sent: the relationship that replaced it was not removed first.".into(),
                );
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
        summary_sentence(
            data.folded.entity_count(),
            data.folded.relationship_count(),
            in_scope.len().max(usize::from(self.scope.is_some())),
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
        if !self.merging() {
            // A fix refused while the merge was out may be asked for again; one that would
            // write to a branch the merge deleted is refused when asked.
            for card in &mut self.cards {
                if matches!(&card.fix, FixState::Refused(reason) if reason == MERGE_RUNNING) {
                    card.fix = FixState::Idle;
                }
            }
        }
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

    /// Whether Merge may run: a review of the whole branch on screen, with no fix out. A review
    /// being read shows a head nobody has seen, a fix still out would land on either side of the
    /// merge, and a review of one commit has not shown what else the merge takes.
    fn mergeable(&self) -> bool {
        matches!(self.load, LoadState::Ready)
            && !self.merging()
            && !matches!(self.merge, MergeState::Merged { .. })
            && self.fixes_running == 0
            && self.scope.is_none()
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
        if !self.mergeable() || self.merge_refusal(cx).is_some() {
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
            let head = match read.await {
                Ok(base) => Some(base.source_commit_id),
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
                session.update(cx, |s, cx| {
                    s.merge(branch.clone(), target.clone(), delete, cx)
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
                    // The head the merge took: the merge commit's second parent, or the commit
                    // a fast-forward moved the target to.
                    let took = match outcome {
                        BranchMergeOutcome::Merged => commit
                            .as_ref()
                            .and_then(|c| c.merged_parent_commit_id.clone()),
                        BranchMergeOutcome::FastForward => {
                            commit.as_ref().map(|c| c.graph_commit_id.clone())
                        }
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
                        cx,
                    )
                    .await;
                    MergeState::Merged {
                        outcome,
                        commit,
                        verified: Check::of(verified),
                        beyond_review,
                        deleted,
                    }
                }
                WriteOutcome::Refused(error) => match error.body.merge_conflicts {
                    Some(conflicts) if !conflicts.is_empty() => MergeState::Conflicts(conflicts),
                    _ => MergeState::Refused(error.body.error),
                },
                WriteOutcome::Unknown { reason } => {
                    this.update(cx, |this, cx| {
                        this.merge = MergeState::Unknown {
                            reason: reason.clone(),
                            landed: Check::Reading,
                        };
                        cx.notify();
                    })
                    .ok();
                    // Settle by reading history, never by merging again (constitution III).
                    let landed =
                        merge_landed(&session, &branch, &target, bound.as_deref(), cx).await;
                    MergeState::Unknown {
                        reason,
                        landed: Check::of(landed),
                    }
                }
            };
            this.update(cx, |this, cx| {
                this.show_conflicts = matches!(state, MergeState::Conflicts(_));
                // The session reads the branches again after a deletion it saw; after an
                // outcome nobody knows, the branch may be gone too.
                if matches!(state, MergeState::Unknown { .. }) {
                    this.session.update(cx, |s, cx| s.reload_branches(cx));
                }
                this.merge = state;
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
        let mergeable = self.mergeable();
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
            let title = if let Some(reason) = merge_refused.clone() {
                reason
            } else if self.scope.is_some() {
                format!("Review all of {}'s commits to merge it", self.branch)
            } else {
                "Merge after checking the branch has not moved".to_owned()
            };
            move |_, cx| Tooltip::with_meta(title.clone(), None, cli.clone(), cx)
        })
        .on_click(cx.listener(|this, _, _, cx| this.merge(cx)));
        let refresh = studio_ui::ds::toolbar_icon_button("refresh", IconName::RotateCw)
            .tooltip(Tooltip::text("Read the branch and its target again"))
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
                .gap_x_4()
                .gap_y_1()
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
        let own_head = self
            .bound_head
            .as_ref()
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
            .menu(move |window, cx| {
                let view = view.clone();
                // Labelled as the menu opens, not at each draw: a branch can hold thousands of
                // commits since its fork.
                let now = studio_ui::now_micros();
                let commits: Vec<(String, String)> = view
                    .upgrade()?
                    .read(cx)
                    .data
                    .as_ref()
                    .map(|data| {
                        data.commits
                            .iter()
                            .map(|commit| {
                                let mut label = short_commit_id(&commit.id);
                                if let Some(actor) = &commit.actor_id {
                                    label.push_str(&format!(" · {actor}"));
                                }
                                label.push_str(&format!(
                                    " · {}",
                                    studio_ui::relative_time(commit.created_at, now)
                                ));
                                (commit.id.clone(), label)
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    let since = view.clone();
                    menu = menu.entry("Since the fork", None, move |_, cx| {
                        since.update(cx, |this, cx| this.set_scope(None, cx)).ok();
                    });
                    if !commits.is_empty() {
                        menu = menu.separator().header("One commit");
                    }
                    for (id, label) in commits {
                        let view = view.clone();
                        menu = menu.entry(label, None, move |_, cx| {
                            view.update(cx, |this, cx| this.set_scope(Some(id.clone()), cx))
                                .ok();
                        });
                    }
                    menu
                }))
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
                let check = match verified {
                    Check::Reading => format!(" Checking {}'s history…", self.target),
                    Check::Shown => format!(" Confirmed in {}'s history.", self.target),
                    Check::NotShown => format!(
                        " Not found in {}'s history yet; open History to check.",
                        self.target
                    ),
                    Check::Unread => format!(
                        " {}'s history could not be read to confirm it; open History to check.",
                        self.target
                    ),
                };
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
                    if matches!(verified, Check::NotShown | Check::Unread)
                        || *beyond_review
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
            MergeState::Unknown { reason, landed } => (
                Severity::Warning,
                match landed {
                    Check::Reading => format!(
                        "The merge outcome is unknown ({reason}). Studio did not retry; reading {}'s history…",
                        self.target
                    ),
                    Check::Shown => format!(
                        "The merge response was lost ({reason}), but {}'s history shows the merge landed.",
                        self.target
                    ),
                    Check::NotShown => format!(
                        "The merge response was lost ({reason}) and {}'s history does not show it. Studio did not retry; merge again when ready.",
                        self.target
                    ),
                    Check::Unread => format!(
                        "The merge outcome is unknown ({reason}), and {}'s history could not be read to tell. Studio did not retry; open History before merging again.",
                        self.target
                    ),
                },
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

        if let Some(own) = group.change {
            let state = &self.cards[own];
            let done = state.fix.done() || self.merging();
            if self.editable(own, cx) {
                let branch = self.branch.clone();
                let focus = focus.clone();
                card = card.action(
                    studio_ui::ds::row_button(("edit", ix), "Edit")
                        .style(ButtonStyle::Subtle)
                        .start_icon(Icon::new(IconName::Pencil))
                        .disabled(done)
                        .tooltip(move |_, cx| {
                            Tooltip::for_action_in(
                                format!("Change what {branch} holds now"),
                                &EditChange,
                                &focus,
                                cx,
                            )
                        })
                        .on_click(
                            cx.listener(move |this, _, window, cx| this.open_edit(own, window, cx)),
                        ),
                );
            }
            let blocked = state.revert_blocked.clone();
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
                let blocked = state
                    .revert_blocked
                    .clone()
                    .or_else(|| was.and_then(|was| was.revert_blocked.clone()));
                let done =
                    state.fix.done() || was.is_some_and(|was| was.fix.done()) || self.merging();
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
/// histories instead.
async fn read_review(
    session: Entity<GraphSession>,
    branch: String,
    target: String,
    scope: Option<String>,
    this: WeakEntity<ReviewView>,
    cx: &mut AsyncApp,
) -> Result<ReviewData, String> {
    if scope.is_none() {
        let base = session
            .update(cx, |session, cx| session.merge_base(&branch, &target, cx))
            .await
            .map_err(|e| e.to_string())?;
        let fork = base.merge_base.graph_commit_id;
        let head = base.source_commit_id;
        if fork == head {
            return Ok(ReviewData {
                fork: Some(fork),
                head: Some(head),
                commits: Vec::new(),
                folded: Arc::new(ChangeFold::new().finish()),
                groups: EntityGroups::default(),
                search: Vec::new().into(),
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
            });
        }
        let start = format!("after:{fork}");
        return match fold_feed(&session, &branch, start, &head, &this, cx).await {
            Ok(fed) => Ok(fed
                .into_review(&session, Some(fork), Some(head), false, cx)
                .await),
            Err(FeedError::Refused) => {
                read_review_since_merge(session, branch, target, fork, head, this, cx).await
            }
            Err(FeedError::Failed(error)) => Err(error),
        };
    }
    read_commit_review(session, branch, target, scope, this, cx).await
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
                let folded = fold.finish();
                let groups = EntityGroups::new(&folded, schema.as_deref());
                let search: Arc<[String]> = folded.changes.iter().map(search_text_of).collect();
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
                    Arc::new(folded),
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
            commits,
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

/// The branch's commits since it forked from `target`, newest first, with the fork and the
/// head.
///
/// The fork is the merge base the server names, and the branch's history is read a page at a
/// time down to it: a page costs only the commits it returns, where the two branches' whole
/// lists cost every commit either holds. A merge base off the branch's own chain (the target
/// was merged into the branch: a commit there names it as its merged parent), or one not
/// reached within [`FORK_PAGES`], is found from both whole lists instead.
async fn commits_since_fork(
    session: &Entity<GraphSession>,
    branch: &str,
    target: &str,
    cx: &mut AsyncApp,
) -> Result<(Option<String>, Option<String>, Vec<ReviewCommit>), String> {
    let base = session
        .update(cx, |session, cx| session.merge_base(branch, target, cx))
        .await
        .map_err(|e| e.to_string())?;
    let fork = base.merge_base.graph_commit_id;
    let mut chain: Vec<CommitOutput> = Vec::new();
    let mut token = None;
    let mut reached = false;
    'pages: for _ in 0..FORK_PAGES {
        let page = session
            .update(cx, |session, cx| {
                session.commit_page(branch, FORK_PAGE, token.take(), cx)
            })
            .await
            .map_err(|e| e.to_string())?;
        for commit in page.commits {
            if commit.graph_commit_id == fork {
                reached = true;
                break 'pages;
            }
            if commit.merged_parent_commit_id.as_deref() == Some(fork.as_str()) {
                break 'pages;
            }
            chain.push(commit);
        }
        match page.next_page_token {
            Some(next) => token = Some(next),
            None => break,
        }
    }
    if !reached {
        return commits_since_fork_by_lists(session, branch, target, None, cx).await;
    }
    let head = chain
        .first()
        .map(|c| c.graph_commit_id.clone())
        .unwrap_or_else(|| fork.clone());
    let since = chain.iter().map(ReviewCommit::from).collect();
    Ok((Some(fork), Some(head), since))
}

/// [`commits_since_fork`] from both branches' whole lists, the session's: the fork is the
/// branch's newest commit the target's list reaches.
async fn commits_since_fork_by_lists(
    session: &Entity<GraphSession>,
    branch: &str,
    target: &str,
    head: Option<&str>,
    cx: &mut AsyncApp,
) -> Result<(Option<String>, Option<String>, Vec<ReviewCommit>), String> {
    // The lists the session keeps, which a commit Studio sees drops: the branch's read again
    // only when it does not reach the reviewed head (or no head says which commit is the
    // head), and the target's as kept, since commits it gained after the fork do not move it.
    let read = |branch: &str, fresh: bool, cx: &mut AsyncApp| {
        session.update(cx, |session, cx| session.commits(branch, fresh, cx))
    };
    let mut branch_commits = read(branch, head.is_none(), cx)
        .await
        .map_err(|e| e.to_string())?;
    if let Some(head) = head
        && !branch_commits
            .iter()
            .any(|commit| commit.graph_commit_id == head)
    {
        branch_commits = read(branch, true, cx).await.map_err(|e| e.to_string())?;
    }
    let target_commits = read(target, false, cx).await.map_err(|e| e.to_string())?;
    // Commits made after the reviewed head are not the review's.
    let from_head = head
        .and_then(|head| {
            branch_commits
                .iter()
                .position(|commit| commit.graph_commit_id == head)
        })
        .unwrap_or(0);
    let branch_commits = &branch_commits[from_head..];
    let fork = fork_point(branch_commits, &target_commits).map(|f| f.graph_commit_id.clone());
    let head = branch_commits.first().map(|h| h.graph_commit_id.clone());
    let since = commits_since(branch_commits, fork.as_deref())
        .iter()
        .map(ReviewCommit::from)
        .collect();
    Ok((fork, head, since))
}

/// Reads a branch the target was merged into since it forked: its feed holds that merge's
/// block, which carries the target's own changes, so the changes are the net ones from the
/// merge base to the reviewed head (`GET /diff`, pinned to both), up to the change cap. The
/// commits are the branch's own since it forked.
async fn read_review_since_merge(
    session: Entity<GraphSession>,
    branch: String,
    target: String,
    base: String,
    head: String,
    this: WeakEntity<ReviewView>,
    cx: &mut AsyncApp,
) -> Result<ReviewData, String> {
    // The target was merged into the branch, so the merge base is off the branch's own chain,
    // and the fork is found from both lists.
    let (_, _, since) =
        commits_since_fork_by_lists(&session, &branch, &target, Some(&head), cx).await?;
    // A range has no single commit: each change is the reviewed head's.
    let cause = ChangeCauseOutput {
        graph_commit_id: head.clone(),
        authored_branch: branch.clone(),
        authored_at: 0,
        parent_commit_id: None,
        merged_parent_commit_id: None,
        actor_id: None,
    };
    let (fold, partial) = fold_pages(
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
    )
    .await?;
    let fed = Fed {
        fold,
        commits: since.into_iter().rev().collect(),
        partial,
        moved_while_reading: false,
    };
    let mut data = fed
        .into_review(&session, Some(base), Some(head), false, cx)
        .await;
    data.merged_in = true;
    Ok(data)
}

/// Reads one commit's own changes, page by page, up to the cap, with the branch's commits since
/// it forked from `target`.
async fn read_commit_review(
    session: Entity<GraphSession>,
    branch: String,
    target: String,
    scope: Option<String>,
    this: WeakEntity<ReviewView>,
    cx: &mut AsyncApp,
) -> Result<ReviewData, String> {
    let (fork, head, since) = commits_since_fork(&session, &branch, &target, cx).await?;
    let Some(commit) = scope else {
        return Err("A review of one commit names the commit.".into());
    };
    let (fold, partial) = fold_pages(
        &this,
        cx,
        async |token: Option<String>, limit: u32, cx: &mut AsyncApp| {
            let page = session
                .update(cx, |session, cx| {
                    session.commit_changes(&commit, token, limit, cx)
                })
                .await
                .map_err(|e| e.to_string())?;
            let block = ChangeBlockOutput {
                cause: page.cause,
                changes: page.changes,
            };
            Ok((block, page.next_page_token))
        },
    )
    .await?;
    let fed = Fed {
        fold,
        commits: since.into_iter().rev().collect(),
        partial,
        moved_while_reading: false,
    };
    Ok(fed.into_review(&session, fork, head, true, cx).await)
}

/// After a lost merge response: whether the target holds the source head.
async fn merge_landed(
    session: &Entity<GraphSession>,
    source: &str,
    target: &str,
    source_head: Option<&str>,
    cx: &mut AsyncApp,
) -> Option<bool> {
    source_head?;
    merge_shown(session, source, target, None, source_head, cx).await
}

/// Whether the target shows the merge of `source_head` (or the merge commit `commit`). The source
/// head being the merge base of the two proves it with one read: merged, fast-forwarded, or
/// already there. When the merge base has moved past it (more of the source was merged since),
/// the target's history is read: the merge commit, or a commit carrying the source head.
async fn merge_shown(
    session: &Entity<GraphSession>,
    source: &str,
    target: &str,
    commit: Option<&str>,
    source_head: Option<&str>,
    cx: &mut AsyncApp,
) -> Option<bool> {
    // The source head being the merge base, or the target at the merge commit, proves it. So
    // does the source's head now being the merge base: the merge took a newer head than the
    // one asked about, and that one's history with it.
    let read = session.update(cx, |session, cx| session.merge_base(source, target, cx));
    if let Ok(base) = read.await
        && (source_head == Some(base.merge_base.graph_commit_id.as_str())
            || commit == Some(base.target_commit_id.as_str())
            || (source_head.is_some() && base.source_commit_id == base.merge_base.graph_commit_id))
    {
        return Some(true);
    }
    let shows = |list: &[CommitOutput]| {
        commit.is_some_and(|c| list.iter().any(|t| t.graph_commit_id == c))
            || source_head.is_some_and(|head| contains_merge_of(list, head))
    };
    // The target's history a page at a time from its head, where a merge just made is: a page
    // costs only the commits it returns. Past [`FORK_PAGES`] pages, the whole list says.
    let mut token = None;
    for _ in 0..FORK_PAGES {
        let read = session.update(cx, |session, cx| {
            session.commit_page(target, FORK_PAGE, token.take(), cx)
        });
        let page = read.await.ok()?;
        if shows(&page.commits) {
            return Some(true);
        }
        match page.next_page_token {
            Some(next) => token = Some(next),
            None => return Some(false),
        }
    }
    let read = session.update(cx, |session, cx| session.commits(target, true, cx));
    let list = read.await.ok()?;
    Some(shows(&list))
}

impl Render for ReviewView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
            .on_action(cx.listener(Self::revert_and_next))
            .on_action(cx.listener(|this, _: &EditChange, window, cx| {
                if this.cards_shown()
                    && let Some(ix) = this.selected
                {
                    this.open_edit(ix, window, cx);
                }
            }))
            // Merge is registered only while the credential may merge (constitution VIII).
            .when(self.merge_refusal(cx).is_none(), |view| {
                view.on_action(cx.listener(|this, _: &Merge, _, cx| this.merge(cx)))
            })
            .child(header)
            .child(body)
    }
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
        let data = ReviewData {
            fork: None,
            head: None,
            commits: Vec::new(),
            search: folded.changes.iter().map(search_text_of).collect(),
            counts: folded.counts(),
            revert_blocked: vec![None; 3],
            by_entity: entity_order(&folded.changes),
            by_key: HashMap::new(),
            card_rows: CardRows::default(),
            marks: folded.changes.iter().map(mark_of).collect(),
            schema: None,
            folded: Arc::new(folded),
            groups,
            partial: false,
            moved_while_reading: false,
            merged_in: false,
        };
        let shown = |query: &str| {
            let matched = search_matches(&data.search, query);
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
            assert!(matches!(view.cards[1].fix, FixState::Refused(_)));
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
    /// is more than was reviewed.
    #[gpui::test]
    async fn a_merge_of_a_head_newer_than_the_review_says_so(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        for (took, beyond) in [("f2", false), ("f3", true)] {
            let (session, _) = review_server(false, None, took, |_| (500, json!({})), cx);
            let (view, cx) = cx.add_window_view(|window, cx| {
                ReviewView::new(session, "feature".into(), "main".into(), window, cx)
            });
            cx.run_until_parked();
            view.update(cx, |view, cx| view.merge(cx));
            cx.run_until_parked();
            view.read_with(cx, |view, _| match &view.merge {
                MergeState::Merged { beyond_review, .. } => assert_eq!(*beyond_review, beyond),
                _ => panic!("not merged"),
            });
        }
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
                assert!(!view.mergeable());
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
        let access = json!({ "graph_id": "knowledge", "actor": "act-dev", "graph_wide": ["read"], "actions": ["read", "change"] });
        let (session, asked) =
            review_server_with(Some(access), false, None, "f2", |_| (500, json!({})), cx);
        let (view, cx) = cx.add_window_view(|window, cx| {
            ReviewView::new(session, "feature".into(), "main".into(), window, cx)
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.focus(&view.focus_handle(cx), cx));
        cx.run_until_parked();
        assert!(!cx.update(|window, cx| window.is_action_available(&Merge, cx)));
        view.update(cx, |view, cx| {
            assert!(view.mergeable(), "the whole review is on screen");
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

    /// A review read while another writer changed the schema is newer once it is in, as is
    /// one read at a head behind the one the session saw; neither sends a dry run for the
    /// head its cards show.
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
        view.update(cx, |view, cx| view.reload(cx));
        cx.run_until_parked();
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
        assert!(!Arc::ptr_eq(&again, &loaded));
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
    fn reviewed_server(
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
        review_server_with(None, merged_in, feed, took, write, cx)
    }

    /// As [`review_server`], the access route answering `access` for every branch; without it,
    /// it answers nothing Studio can read, so what the credential may do stays unknown.
    fn review_server_with(
        access: Option<Value>,
        merged_in: bool,
        feed: Option<Value>,
        took: &'static str,
        write: impl Fn(usize) -> (u16, Value) + Send + Sync + 'static,
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
                if let Some(access) = access.as_ref().filter(|_| path.ends_with("/access")) {
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
                        json!({ "error": "start commit is not on this branch's first-parent chain", "code": "bad_request" }),
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
    /// net diff from the base instead, and the lists for the branch's commits.
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
        assert_eq!(
            asked
                .iter()
                .filter(|request| request.contains("/commits"))
                .count(),
            2,
            "both lists: {asked:?}"
        );
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
                &mut async_cx
            )
            .await,
            Some(true)
        );
        assert_eq!(
            merge_landed(&session, "feature", "main", Some("f2"), &mut async_cx).await,
            Some(true),
            "f2 was merged, though the merge base is f3 now"
        );
        assert_eq!(
            merge_landed(&session, "feature", "main", Some("f9"), &mut async_cx).await,
            Some(true),
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
            merge_landed(&session, "feature", "main", Some("f9"), &mut async_cx).await,
            Some(false)
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
            &mut async_cx,
        )
        .await;
        assert_eq!(Check::of(shown), Check::Unread);
    }
}
