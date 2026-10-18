//! The review tab (spec 008): what a branch changed since it forked from its target, folded per
//! entity, with fixes bound to the reviewed head and a merge that checks the head first.

use std::collections::{HashSet, VecDeque};

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
use graph_store::fixes::compose;
use graph_store::{
    ChangeFold, EntityGroup, EntityGroups, FoldedChange, FoldedChanges, GraphSession, GroupRun,
    GroupSection, HeadWatch, NetChange, NodeNames, REVIEW_CHANGE_CAP, RelationshipEntry,
    SessionEvent, TypeCounts, commits_since, contains_merge_of, entity_name, fork_point,
    revert_query, summary_sentence,
};
use serde_json::Value;
use studio_actions::review::{
    AcceptAndNext, EditChange, Merge, NextChange, PreviousChange, RevertAndNext,
};
use studio_ui::ds::Button;
use studio_ui::ds::{radius, space};
use studio_ui::menu::ContextMenu;
use studio_ui::{
    ChangeCard, ChangeKind, PropertyRow, RelationshipLine, change_counts, change_group_header,
    group_digits, short_commit_id, type_swatch,
};
use studio_workspace::{Item, ItemEvent, SavedItem, Workspace, workspace_for};
use ui::prelude::*;
use ui::{Banner, Checkbox, KeyBinding, PopoverMenu, Severity, ToggleState, Tooltip};

use crate::conflicts::{conflict_wording, resolution_hint};

/// How long typing in the search pauses before the cards are filtered.
const SEARCH_DELAY: std::time::Duration = std::time::Duration::from_millis(80);

/// Changes asked for per change-feed page.
const FEED_PAGE: usize = 1_000;

/// One row of the review's list.
#[derive(Debug, Clone, Copy)]
enum Row {
    /// The header of a run of one type's cards, by its index in the view's runs.
    Header(usize),
    /// An entity's card, by its group in [`ReviewData::groups`].
    Entity(usize),
}

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

struct ReviewData {
    /// The merge base: the newest commit of the branch's history the target holds.
    fork: Option<String>,
    /// The branch head the review read (the reviewed head).
    head: Option<String>,
    /// The branch's commits since the fork, newest first.
    commits: Vec<ReviewCommit>,
    folded: FoldedChanges,
    /// The changes by entity: each node with the relationship changes reviewed on its card.
    groups: EntityGroups,
    /// Each change's search text, lowercase, made when it was folded: a search goes through
    /// these, not through every value of every change on each key.
    search: Vec<String>,
    counts: Vec<TypeCounts>,
    /// Why each change cannot be reverted, when it cannot: worked out with the fold, off the
    /// main thread, since a review holds up to 20,000 changes.
    revert_blocked: Vec<Option<String>>,
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
}

/// What a reviewed mark is kept by: the entity and the last commit that changed it.
fn mark_key(change: &FoldedChange) -> String {
    format!(
        "{}@{}",
        change.key(),
        change.commits.last().map_or("", String::as_str)
    )
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

enum LoadState {
    Loading { folded: usize },
    Failed(SharedString),
    Ready,
}

pub struct ReviewView {
    session: Entity<GraphSession>,
    branch: String,
    target: String,
    /// `None` reviews everything since the fork; `Some` one commit of the branch.
    scope: Option<String>,
    /// Narrows the cards to those whose id, type or values contain the text.
    search: Entity<Editor>,
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
    list_state: ListState,
    /// The head fixes and the merge bind to: the reviewed head, then each fix's own commit.
    bound_head: Option<String>,
    /// Reviewed marks, by entity and the last commit that changed it, kept across Refresh: a
    /// change made again since is reviewed again.
    reviewed_keys: HashSet<String>,
    merge: MergeState,
    /// Whether the merge deletes the branch once it lands; on unless the person turns it off.
    delete_branch: bool,
    show_conflicts: bool,
    /// Someone else wrote to the branch after this review was read.
    newer: bool,
    /// Fixes asked for and not yet back, the one out and those waiting for it.
    fixes_running: usize,
    /// Fixes waiting for the one out: each is sent bound to the head the one before left.
    fix_queue: VecDeque<Fix>,
    fix_out: bool,
    /// The session saw the branch's head move while a fix or the merge was out; compared once
    /// they are back, since each moves the bound head itself.
    head_moved_while_writing: bool,
    /// The session asks after the reviewed branch's head while the review is open.
    _head_watch: HeadWatch,
    _load_task: Option<Task<()>>,
    _names_task: Option<Task<()>>,
    _merge_task: Option<Task<()>>,
    _search_task: Option<Task<()>>,
    _fix_tasks: Vec<Task<()>>,
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
        Some(SavedItem {
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
                this._search_task = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(SEARCH_DELAY).await;
                    this.update(cx, |this, cx| this.apply_search(cx)).ok();
                }));
            }
        });
        let subscription =
            cx.subscribe(&session, |this, _, event: &SessionEvent, cx| match event {
                SessionEvent::HeadMoved { branch, .. } if *branch == this.branch => {
                    this.compare_head(cx)
                }
                SessionEvent::SchemaChanged => cx.notify(),
                _ => {}
            });
        let head_watch = session.update(cx, |session, cx| session.watch_head(&branch, cx));
        let mut view = Self {
            session,
            branch,
            target,
            scope: None,
            search,
            focus_handle: cx.focus_handle(),
            load: LoadState::Loading { folded: 0 },
            data: None,
            cards: Vec::new(),
            rows: Vec::new(),
            headers: Vec::new(),
            selected: None,
            names: NodeNames::default(),
            list_state: ListState::new(0, ListAlignment::Top, px(800.)),
            bound_head: None,
            reviewed_keys: HashSet::new(),
            merge: MergeState::Idle,
            delete_branch: true,
            show_conflicts: false,
            newer: false,
            fixes_running: 0,
            fix_queue: VecDeque::new(),
            fix_out: false,
            head_moved_while_writing: false,
            _head_watch: head_watch,
            _load_task: None,
            _names_task: None,
            _merge_task: None,
            _search_task: None,
            _fix_tasks: Vec::new(),
            _subscriptions: vec![subscription, search_subscription],
        };
        view.reload(cx);
        view
    }

    fn reviewed_head(&self) -> Option<String> {
        self.data.as_ref().and_then(|d| d.head.clone())
    }

    /// Reads the review again from the branch head. Reviewed marks are kept.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        for (state, change) in self
            .cards
            .iter()
            .zip(self.data.iter().flat_map(|d| d.folded.changes.iter()))
        {
            if state.reviewed {
                self.reviewed_keys.insert(mark_key(change));
            } else {
                self.reviewed_keys.remove(&mark_key(change));
            }
        }
        self.load = LoadState::Loading { folded: 0 };
        self.newer = false;
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
            this.update(cx, |this, cx| this.finish_load(result, cx))
                .ok();
        }));
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    fn finish_load(&mut self, result: Result<ReviewData, String>, cx: &mut Context<Self>) {
        match result {
            Ok(data) => {
                self.cards = data
                    .folded
                    .changes
                    .iter()
                    .zip(&data.revert_blocked)
                    .map(|(change, blocked)| CardState {
                        reviewed: self.reviewed_keys.contains(&mark_key(change)),
                        fix: FixState::Idle,
                        revert_blocked: blocked.clone(),
                    })
                    .collect();
                (self.rows, self.headers) = build_rows(&data, &self.search_text(cx));
                self.list_state.reset(self.rows.len());
                self.selected = self
                    .visible_groups()
                    .first()
                    .and_then(|&group| data.groups.groups[group].primary());
                self.bound_head = data.head.clone();
                self.newer = data.moved_while_reading;
                let ends = data.groups.unnamed_ends();
                let head = data.head.clone();
                self.data = Some(data);
                self.load = LoadState::Ready;
                self.read_names(ends, head, cx);
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

    /// Shows only the cards matching the search, keeping the selection when its card still
    /// shows.
    fn apply_search(&mut self, cx: &mut Context<Self>) {
        let Some(data) = &self.data else { return };
        (self.rows, self.headers) = build_rows(data, &self.search_text(cx));
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
        cx.notify();
    }

    /// Reviews everything since the fork (`None`) or one commit, reading again.
    fn set_scope(&mut self, scope: Option<String>, cx: &mut Context<Self>) {
        if self.scope != scope {
            self.scope = scope;
            self.reload(cx);
        }
    }

    /// Marks the selected entity's card reviewed, every change on it, and moves to the next.
    fn accept_and_next(&mut self, _: &AcceptAndNext, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(group) = self.selected.and_then(|s| self.group_of(s)) {
            self.set_group_reviewed(group, true);
            self.select_next(&NextChange, window, cx);
        }
    }

    fn revert_and_next(&mut self, _: &RevertAndNext, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.selected {
            self.revert(ix, cx);
            self.select_next(&NextChange, window, cx);
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
        // A fix during a merge could land on either side of it.
        if self.merging() {
            return;
        }
        let Some(data) = &self.data else { return };
        let Some(change) = data.folded.changes.get(ix) else {
            return;
        };
        let Some(card) = self.cards.get(ix) else {
            return;
        };
        if card.fix.done() {
            return;
        }
        let Some(schema) = self.session.read(cx).schema_model() else {
            self.cards[ix].fix = FixState::Refused("The schema has not loaded yet.".into());
            cx.notify();
            return;
        };
        let query = match revert_query(&schema, change) {
            Ok(query) => query,
            Err(reason) => {
                self.cards[ix].revert_blocked = Some(reason.0);
                cx.notify();
                return;
            }
        };
        let request = ChangeRequest {
            query: query.text.clone(),
            name: Some(query.name.clone()),
            params: query.params_value(),
            branch: Some(self.branch.clone()),
        };
        let fix = Fix {
            key: change.key(),
            request,
            description: format!("Revert {} {}", change.type_name, change.id),
            edited: false,
        };
        self.cards[ix].fix = FixState::Running;
        self.queue_fix(fix, cx);
    }

    fn merging(&self) -> bool {
        matches!(self.merge, MergeState::Checking | MergeState::Merging)
    }

    /// Sends `fix` once the fixes before it are back. Each is bound to the head the one before
    /// left, so a second fix asked for quickly is not refused as "moved" by the first.
    fn queue_fix(&mut self, fix: Fix, cx: &mut Context<Self>) {
        self.fixes_running += 1;
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
        }) = self.fix_queue.pop_front()
        else {
            return;
        };
        self.fix_out = true;
        let if_commit = self.bound_head.clone();
        let task = self.session.update(cx, |session, cx| {
            session.write_request(request, if_commit, description, cx)
        });
        self._fix_tasks.push(cx.spawn(async move |this, cx| {
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

    /// Writes edited fields on the reviewed branch, each bound to the value the review showed,
    /// in one mutation bound to the head this review holds. Never retried.
    pub(crate) fn save_edit(
        &mut self,
        key: &str,
        changes: Vec<(String, Value, Value)>,
        cx: &mut Context<Self>,
    ) {
        // The form was filled from the change with this key; the review may have been read
        // again since, and its cards moved.
        if self.merging() {
            return;
        }
        let Some(ix) = self.card_of(key) else {
            return;
        };
        let Some(change) = self.data.as_ref().and_then(|d| d.folded.changes.get(ix)) else {
            return;
        };
        let Some(schema) = self.session.read(cx).schema_model() else {
            return;
        };
        let parts: Result<Vec<_>, _> = changes
            .iter()
            .map(|(property, old, new)| {
                gq_language::builder::update_cell(
                    &schema,
                    &change.type_name,
                    property,
                    &change.id,
                    old,
                    new,
                )
            })
            .collect();
        let query = match parts {
            Ok(parts) => compose("studio_edit", parts),
            Err(error) => {
                self.cards[ix].fix = FixState::Refused(error.to_string());
                cx.notify();
                return;
            }
        };
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
        };
        self.cards[ix].fix = FixState::Saving;
        self.queue_fix(fix, cx);
    }

    /// The card of the change with `key` in the review as it is now.
    fn card_of(&self, key: &str) -> Option<usize> {
        self.data
            .as_ref()?
            .folded
            .changes
            .iter()
            .position(|change| change.key() == key)
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
        let fix = match outcome {
            WriteOutcome::Committed(output) => match output.commit {
                Some(commit) => {
                    self.bound_head = Some(commit.graph_commit_id.clone());
                    let commit = commit.graph_commit_id;
                    if edited {
                        FixState::Edited { commit }
                    } else {
                        FixState::Reverted { commit }
                    }
                }
                None => FixState::Unchanged,
            },
            WriteOutcome::Refused(error) if error.status == 412 => FixState::Moved,
            WriteOutcome::Refused(error) => FixState::Refused(error.body.error),
            WriteOutcome::Unknown { .. } => FixState::Unknown,
        };
        // Found by the change's key: a Refresh while the fix was out may have moved its card.
        if let Some(card) = self.card_of(key).and_then(|ix| self.cards.get_mut(ix)) {
            if matches!(fix, FixState::Reverted { .. }) {
                card.reviewed = true;
            }
            card.fix = fix;
        }
        self.send_next_fix(cx);
        self.writes_settled(cx);
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
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
            self.newer = true;
            cx.emit(ItemEvent::UpdateTab);
            cx.notify();
        }
    }

    fn writes_settled(&mut self, cx: &mut Context<Self>) {
        if self.fixes_running == 0
            && !matches!(self.merge, MergeState::Merging)
            && std::mem::take(&mut self.head_moved_while_writing)
        {
            self.compare_head(cx);
        }
    }

    /// Whether Merge may run: a review of the whole branch on screen, with no fix out. A review
    /// being read shows a head nobody has seen, a fix still out would land on either side of the
    /// merge, and a review of one commit has not shown what else the merge takes.
    fn mergeable(&self) -> bool {
        matches!(self.load, LoadState::Ready)
            && !self.merging()
            && self.fixes_running == 0
            && self.scope.is_none()
    }

    /// Whether the merge deletes the branch: as the checkbox says, and never `main`.
    fn delete_after_merge(&self) -> bool {
        self.delete_branch && self.branch != "main"
    }

    /// Merges the branch into the target after checking the branch still sits at the head this
    /// review holds. The outcome is shown, then confirmed against the target's history.
    pub fn merge(&mut self, cx: &mut Context<Self>) {
        if !self.mergeable() {
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
                        cx.notify();
                    })
                    .ok();
                    return;
                }
            };
            if head != bound {
                this.update(cx, |this, cx| {
                    this.merge = MergeState::Moved { head };
                    this.newer = true;
                    cx.emit(ItemEvent::UpdateTab);
                    cx.notify();
                })
                .ok();
                return;
            }
            let Ok(task) = this.update(cx, |this, cx| {
                this.merge = MergeState::Merging;
                cx.notify();
                let delete = this.delete_after_merge();
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
                this.merge = state;
                this.writes_settled(cx);
                this.session.update(cx, |s, cx| s.reload_branches(cx));
                cx.emit(ItemEvent::UpdateTab);
                cx.notify();
            })
            .ok();
        }));
    }

    fn open_conflict(&mut self, conflict: &MergeConflictOutput, cx: &mut Context<Self>) {
        let Some(id) = &conflict.entity_id else {
            return;
        };
        let Some(data) = &self.data else { return };
        if let Some(ix) = data
            .folded
            .changes
            .iter()
            .position(|c| &c.id == id && c.type_name == conflict.type_name)
        {
            self.show_conflicts = false;
            self.select(ix, cx);
        }
    }

    fn net_of(&self, type_name: &str, id: Option<&str>) -> Option<NetChange> {
        let id = id?;
        self.data
            .as_ref()?
            .folded
            .changes
            .iter()
            .find(|c| c.id == id && c.type_name == type_name)
            .and_then(FoldedChange::net)
    }

    // ---- rendering ----

    fn render_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        let border = colors.border_variant;
        let focus = self.focus_handle.clone();
        let mergeable = self.mergeable();
        let merge_button = studio_ui::ds::button(
            "merge",
            format!("Merge {} into {}", self.branch, self.target),
        )
        .style(ButtonStyle::Filled)
        .label_size(LabelSize::Small)
        .start_icon(Icon::new(IconName::GitBranch).size(IconSize::Small))
        .key_binding(KeyBinding::for_action_in(&Merge, &focus, cx))
        .disabled(!mergeable)
        .tooltip({
            let cli = format!(
                "omnigraph branch merge {} --into {}{}",
                self.branch,
                self.target,
                if self.delete_after_merge() {
                    " --delete-branch"
                } else {
                    ""
                }
            );
            // A disabled Merge says why (constitution VI).
            let title = if self.scope.is_some() {
                format!("Review all of {}'s commits to merge it", self.branch)
            } else {
                "Merge after checking the branch has not moved".to_owned()
            };
            move |_, cx| Tooltip::with_meta(title.clone(), None, cli.clone(), cx)
        })
        .on_click(cx.listener(|this, _, _, cx| this.merge(cx)));
        let refresh = studio_ui::ds::button("refresh", "Refresh")
            .style(ButtonStyle::Subtle)
            .label_size(LabelSize::Small)
            .start_icon(Icon::new(IconName::RotateCw).size(IconSize::Small))
            .on_click(cx.listener(|this, _, _, cx| this.reload(cx)));

        let (sentence, detail) = match (&self.load, &self.data) {
            (LoadState::Ready, Some(data)) => {
                // The commits in scope: all since the fork, or the one chosen.
                let in_scope: Vec<&ReviewCommit> = data
                    .commits
                    .iter()
                    .filter(|c| self.scope.as_ref().is_none_or(|id| *id == c.id))
                    .collect();
                let actors: Vec<String> = {
                    let mut actors: Vec<String> =
                        in_scope.iter().filter_map(|c| c.actor_id.clone()).collect();
                    actors.sort();
                    actors.dedup();
                    actors
                };
                let sentence = summary_sentence(
                    data.folded.entity_count(),
                    data.folded.relationship_count(),
                    in_scope.len().max(usize::from(self.scope.is_some())),
                    &actors,
                );
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
                        .child(studio_ui::kind_swatch(&count.type_name, count.is_edge))
                        .child(Label::new(count.type_name.clone()).size(LabelSize::Small))
                        .child(change_counts(count.added, count.changed, count.removed))
                }))
        });

        let mut banners: Vec<AnyElement> = Vec::new();
        if self.data.as_ref().is_some_and(|data| data.partial) {
            let text = format!(
                "Partial: showing the first {} changes. The full branch diff needs a server route (E6).",
                group_digits(REVIEW_CHANGE_CAP)
            );
            banners.push(
                Banner::new()
                    .severity(Severity::Warning)
                    .child(Label::new(text).size(LabelSize::Small))
                    .into_any_element(),
            );
        }
        let own_head = self
            .bound_head
            .as_ref()
            .filter(|head| Some(*head) != self.reviewed_head().as_ref());
        if let (Some(head), false) = (own_head, self.newer) {
            banners.push(
                Banner::new()
                    .severity(Severity::Info)
                    .child(
                        Label::new(format!(
                            "Your fixes moved {} to {}. Cards show the review as read; merging uses the new head.",
                            self.branch,
                            short_commit_id(head)
                        ))
                        .size(LabelSize::Small),
                    )
                    .action_slot(
                        studio_ui::ds::button("refresh-own", "Refresh")
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
                    )
                    .into_any_element(),
            );
        }
        if self.newer && !matches!(self.merge, MergeState::Moved { .. }) {
            banners.push(
                Banner::new()
                    .severity(Severity::Info)
                    .child(
                        Label::new(format!(
                            "Newer data: {} changed after this review was read.",
                            self.branch
                        ))
                        .size(LabelSize::Small),
                    )
                    .action_slot(
                        studio_ui::ds::button("refresh-newer", "Refresh")
                            .label_size(LabelSize::Small)
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
        let commits: Vec<(String, String)> = self
            .data
            .as_ref()
            .map(|data| {
                let now = studio_ui::now_micros();
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
        let scope_label = match &self.scope {
            None => "Since the fork".to_owned(),
            Some(id) => format!("Commit {}", short_commit_id(id)),
        };
        let view = cx.entity().downgrade();
        let scope_menu = PopoverMenu::new("review-scope")
            .trigger(
                studio_ui::ds::button("review-scope-button", scope_label)
                    .style(ButtonStyle::Outlined)
                    .label_size(LabelSize::Small)
                    .end_icon(
                        Icon::new(IconName::ChevronDown)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .menu(move |window, cx| {
                let view = view.clone();
                let commits = commits.clone();
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
        let search = studio_ui::ds::search_field(self.search.clone(), cx).w(px(280.));

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
                    .child(Icon::new(IconName::GitBranch).color(Color::Muted))
                    .child(
                        Headline::new(format!("{} → {}", self.branch, self.target))
                            .size(HeadlineSize::Small),
                    )
                    .child(div().flex_1())
                    .child(refresh)
                    .when(self.branch != "main", |row| {
                        row.child(
                            Checkbox::new(
                                "delete-after-merge",
                                ToggleState::from(self.delete_branch),
                            )
                            .label(format!("Delete {}", self.branch))
                            .label_size(LabelSize::Small)
                            .disabled(!mergeable)
                            .on_click(cx.listener(
                                |this, state: &ToggleState, _, cx| {
                                    this.delete_branch = state.selected();
                                    cx.notify();
                                },
                            )),
                        )
                    })
                    .child(merge_button),
            )
            .child(h_flex().gap(px(space::MD)).child(scope_menu).child(search))
            .child(Label::new(sentence))
            .when(!detail.is_empty(), |d| {
                d.child(
                    Label::new(detail)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .children(counts)
            .children(banners)
            .into_any_element()
    }

    fn render_merge_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (severity, text, action): (Severity, String, Option<AnyElement>) = match &self.merge {
            MergeState::Idle => return None,
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
                        .label_size(LabelSize::Small)
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
                    "{} conflict{} stopped the merge. {}",
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
                    .label_size(LabelSize::Small)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_conflicts = !this.show_conflicts;
                        cx.notify();
                    }))
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
        let mut banner = Banner::new()
            .severity(severity)
            .wrap_content(true)
            .child(Label::new(text).size(LabelSize::Small));
        if let Some(action) = action {
            banner = banner.action_slot(action);
        }
        Some(banner.into_any_element())
    }

    fn render_conflicts(&self, cx: &mut Context<Self>) -> AnyElement {
        let MergeState::Conflicts(conflicts) = &self.merge else {
            return div().into_any_element();
        };
        let colors = cx.theme().colors();
        let surface = colors.surface_background;
        let border = colors.border_variant;
        v_flex()
            .id("conflicts")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .p(px(space::XL))
            .gap(px(space::MD))
            .child(
                Label::new(format!(
                    "Conflicts merging {} into {}",
                    self.branch, self.target
                ))
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .children(conflicts.iter().enumerate().map(|(ix, conflict)| {
                let here = self.net_of(&conflict.type_name, conflict.entity_id.as_deref());
                let words = conflict_wording(conflict.kind, here, &self.target);
                let entity = conflict.entity_id.clone().unwrap_or_else(|| "—".into());
                let conflict_for_click = conflict.clone();
                let reviewable = here.is_some();
                h_flex()
                    .id(("conflict", ix))
                    .w_full()
                    .gap(px(space::LG))
                    .px(px(space::LG))
                    .py(px(space::MD))
                    .rounded(px(radius::MD))
                    .border_1()
                    .border_color(border)
                    .bg(surface)
                    .child(Icon::new(IconName::GitMergeConflict).color(Color::Conflict))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(Label::new(words).color(Color::Conflict))
                            .child(
                                h_flex()
                                    .gap(px(space::MD))
                                    .child(type_swatch(&conflict.type_name))
                                    .child(
                                        Label::new(conflict.type_name.clone())
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    )
                                    .child(
                                        Label::new(entity).size(LabelSize::Small).buffer_font(cx),
                                    ),
                            )
                            .child(
                                Label::new(conflict.message.clone())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    )
                    .when(reviewable, |d| {
                        d.cursor_pointer()
                            .child(
                                Label::new("Show change")
                                    .size(LabelSize::XSmall)
                                    .color(Color::Accent),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.open_conflict(&conflict_for_click, cx)
                            }))
                    })
            }))
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
            Some(Row::Header(ix)) => {
                let header = &self.headers[ix];
                let title = header.section.map(|section| match section {
                    GroupSection::Existing => format!("Changes to what {} has", self.target),
                    GroupSection::Added => format!("Added on {}", self.branch),
                });
                v_flex()
                    .px(px(space::XL))
                    .children(title.map(|title| {
                        div()
                            .pt(px(space::XL))
                            .child(Label::new(title).size(LabelSize::Small).color(Color::Muted))
                    }))
                    .child(
                        change_group_header(
                            &header.type_name,
                            header.is_edge,
                            header.added,
                            header.changed,
                            header.removed,
                        )
                        .when(header.relationships > 0, |d| {
                            d.child(
                                Label::new(match header.relationships {
                                    1 => "· 1 relationship".to_owned(),
                                    n => format!("· {} relationships", group_digits(n)),
                                })
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                            )
                        }),
                    )
                    .into_any_element()
            }
            Some(Row::Entity(group)) => {
                let schema = self.session.read(cx).schema_model();
                let card = self.entity_card(data, group, schema.as_deref(), cx);
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
        let changes = &data.folded.changes;
        let focus = self.focus_handle.clone();
        let selected_group = self.selected.and_then(|s| data.groups.home.get(s).copied());
        let unreviewed = self.group_unreviewed(group);
        let total = group.changes().count();
        let reviewed = unreviewed == 0;

        let mut card = match group.change {
            Some(own) => change_card(&changes[own], schema, ("change", ix))
                .name(entity_name(&changes[own], schema)),
            None => ChangeCard::new(("change", ix), ChangeKind::Unchanged, group.id.clone()).name(
                self.names
                    .get(&(group.type_name.clone(), group.id.clone()))
                    .cloned(),
            ),
        }
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

        let review_button = Button::new(
            ("reviewed", ix),
            if reviewed {
                "Reviewed"
            } else {
                "Mark reviewed"
            },
        )
        .label_size(LabelSize::XSmall)
        .style(ButtonStyle::Subtle)
        .toggle_state(reviewed)
        .when(reviewed, |b| {
            b.start_icon(Icon::new(IconName::Check).size(IconSize::XSmall))
        })
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
            let done = state.fix.done();
            if self.editable(own, cx) {
                let branch = self.branch.clone();
                let focus = focus.clone();
                card = card.action(
                    Button::new(("edit", ix), "Edit")
                        .label_size(LabelSize::XSmall)
                        .style(ButtonStyle::Subtle)
                        .start_icon(Icon::new(IconName::Pencil).size(IconSize::XSmall))
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
                Button::new(("revert", ix), "Revert")
                    .label_size(LabelSize::XSmall)
                    .style(ButtonStyle::Subtle)
                    .start_icon(Icon::new(IconName::Undo).size(IconSize::XSmall))
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

        let entries = data.groups.entries(&data.folded, ix, schema, &self.names);
        let counts = relationship_counts(&entries);
        let lines = entries
            .into_iter()
            .map(|entry| {
                let change = entry.change;
                let mirror = entry.net.is_none();
                let mut line = relationship_line(entry, &changes[change], schema)
                    .on_click(cx.listener(move |this, _, _, cx| this.select(change, cx)));
                if mirror {
                    return line;
                }
                let state = &self.cards[change];
                let hover = SharedString::from(format!("relationship-{change}"));
                let blocked = state.revert_blocked.clone();
                let branch = self.branch.clone();
                let revert =
                    studio_ui::ds::icon_button(("revert-relationship", change), IconName::Undo)
                        .icon_size(IconSize::XSmall)
                        .style(ButtonStyle::Subtle)
                        .visible_on_hover(hover.clone())
                        .disabled(blocked.is_some() || state.fix.done())
                        .tooltip(move |_, cx| match &blocked {
                            Some(reason) => Tooltip::simple(reason.clone(), cx),
                            None => Tooltip::with_meta(
                                "Revert this relationship change",
                                Some(&RevertAndNext),
                                format!("Writes to {branch}, bound to the reviewed head"),
                                cx,
                            ),
                        })
                        .on_click(cx.listener(move |this, _, _, cx| this.revert(change, cx)));
                // The selected change is what ⌥⌘Z reverts: a card without its own change selects
                // its first relationship, which shows it.
                line = line
                    .selected(self.selected == Some(change))
                    .hollow(state.reviewed)
                    .hover_group(hover)
                    .action(revert);
                if let Some((text, color)) = fix_status(&state.fix) {
                    line = line.status(text, color);
                }
                line
            })
            .collect();
        card = card.relationships(counts, lines);
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

/// Added, changed and removed relationships among a card's entries; mirrors are counted on
/// their own card.
pub(crate) fn relationship_counts(entries: &[RelationshipEntry]) -> (usize, usize, usize) {
    entries
        .iter()
        .fold((0, 0, 0), |(added, changed, removed), entry| {
            match entry.net {
                Some(NetChange::Added) => (added + 1, changed, removed),
                Some(NetChange::Changed) => (added, changed + 1, removed),
                Some(NetChange::Removed) => (added, changed, removed + 1),
                None => (added, changed, removed),
            }
        })
}

/// A card's line for one relationship entry: what changed, which way it points, the other
/// end, and a changed relationship's property rows. A mirror says where it is reviewed.
pub(crate) fn relationship_line(
    entry: RelationshipEntry,
    edge: &FoldedChange,
    schema: Option<&SchemaModel>,
) -> RelationshipLine {
    let kind = entry.net.map(|net| match net {
        NetChange::Added => ChangeKind::Added,
        NetChange::Changed => ChangeKind::Changed,
        NetChange::Removed => ChangeKind::Removed,
    });
    let id = if kind.is_some() {
        ("relationship", entry.change)
    } else {
        ("mirror", entry.change)
    };
    let rows = if kind.is_some() {
        edge.card_diffs(schema)
            .into_iter()
            .map(|diff| PropertyRow {
                name: diff.name.into(),
                before: diff.before,
                after: diff.after,
            })
            .collect()
    } else {
        Vec::new()
    };
    let note = match kind {
        // The other end is the card it is reviewed on.
        None => Some("reviewed there".to_owned()),
        Some(_) if entry.other_is_new => Some("new".to_owned()),
        Some(_) => None,
    };
    let line = RelationshipLine::new(id, kind, entry.words, entry.outgoing, entry.other)
        .other_detail(entry.other_type)
        .rows(rows);
    match note {
        Some(note) => line.note(note),
        None => line,
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

/// The card for one folded change.
pub(crate) fn change_card(
    change: &FoldedChange,
    schema: Option<&SchemaModel>,
    id: impl Into<gpui::ElementId>,
) -> ChangeCard {
    let kind = match change.net() {
        Some(NetChange::Added) => ChangeKind::Added,
        Some(NetChange::Removed) => ChangeKind::Removed,
        _ => ChangeKind::Changed,
    };
    let rows = change
        .card_diffs(schema)
        .into_iter()
        .map(|diff| PropertyRow {
            name: diff.name.into(),
            before: diff.before,
            after: diff.after,
        })
        .collect();
    let mut card = ChangeCard::new(id, kind, change.id.clone()).rows(rows);
    if let Some((from, to)) = change.endpoints() {
        card = card.endpoints(from, to);
    }
    card
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

/// The list's rows: each card the search leaves, under a header for each run of one type, the
/// first of a section carrying its title. A card matches when its own change or a
/// relationship reviewed on it does. The headers count what the run shows.
fn build_rows(data: &ReviewData, query: &str) -> (Vec<Row>, Vec<GroupRun>) {
    let matches = |change: usize| {
        query.is_empty()
            || data
                .search
                .get(change)
                .is_some_and(|text| text.contains(query))
    };
    let runs = data
        .groups
        .runs(&data.folded, |group| group.changes().any(matches));
    let mut rows = Vec::new();
    for (ix, run) in runs.iter().enumerate() {
        rows.push(Row::Header(ix));
        rows.extend(run.groups.iter().map(|&group| Row::Entity(group)));
    }
    (rows, runs)
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
                folded: ChangeFold::new().finish(),
                groups: EntityGroups::default(),
                search: Vec::new(),
                counts: Vec::new(),
                revert_blocked: Vec::new(),
                partial: false,
                moved_while_reading: false,
                merged_in: false,
            });
        }
        let start = format!("after:{fork}");
        return match fold_feed(&session, &branch, start, &head, &this, cx).await {
            Ok(fed) => Ok(fed.into_review(&session, Some(fork), Some(head), cx).await),
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
    async fn into_review(
        self,
        session: &Entity<GraphSession>,
        fork: Option<String>,
        head: Option<String>,
        cx: &mut AsyncApp,
    ) -> ReviewData {
        let fold = self.fold;
        let schema = session.read_with(cx, |session, _| session.schema_model());
        let (folded, groups, search, revert_blocked) = cx
            .background_executor()
            .spawn(async move {
                let folded = fold.finish();
                let groups = EntityGroups::new(&folded, schema.as_deref());
                let search = folded.changes.iter().map(search_text_of).collect();
                let revert_blocked = folded
                    .changes
                    .iter()
                    .map(|change| {
                        let schema = schema.as_ref()?;
                        revert_query(schema, change).err().map(|e| e.0)
                    })
                    .collect();
                (folded, groups, search, revert_blocked)
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
        // Fold off the main thread (constitution IX).
        let (folded, complete) = cx
            .background_executor()
            .spawn(async move {
                let mut complete = true;
                for block in blocks {
                    complete &= fold.add_block_capped(block, REVIEW_CHANGE_CAP);
                }
                (fold, complete)
            })
            .await;
        fold = folded;
        let raw = fold.raw_changes();
        this.update(cx, |this, cx| {
            this.load = LoadState::Loading { folded: raw };
            cx.notify();
        })
        .ok();
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

/// The branch's commits since it forked from `target`, newest first, up to `head` when the
/// branch holds it, with the fork: read now, not from the session's shared lists, since a
/// review shows what the branches hold.
async fn commits_since_fork(
    session: &Entity<GraphSession>,
    branch: &str,
    target: &str,
    head: Option<&str>,
    cx: &mut AsyncApp,
) -> Result<(Option<String>, Option<String>, Vec<ReviewCommit>), String> {
    let branch_commits = session
        .update(cx, |session, cx| session.commits(branch, true, cx))
        .await
        .map_err(|e| e.to_string())?;
    let target_commits = session
        .update(cx, |session, cx| session.commits(target, true, cx))
        .await
        .map_err(|e| e.to_string())?;
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
    let (_, _, since) = commits_since_fork(&session, &branch, &target, Some(&head), cx).await?;
    // A range has no single commit: each change is the reviewed head's.
    let cause = ChangeCauseOutput {
        graph_commit_id: head.clone(),
        authored_branch: branch.clone(),
        authored_at: 0,
        parent_commit_id: None,
        merged_parent_commit_id: None,
        actor_id: None,
    };
    let mut fold = ChangeFold::new();
    let mut partial = false;
    let mut token: Option<String> = None;
    loop {
        let limit = FEED_PAGE.min(REVIEW_CHANGE_CAP - fold.raw_changes()).max(1) as u32;
        let page = session
            .update(cx, |session, cx| {
                session.diff(&base, &head, token.clone(), limit, cx)
            })
            .await
            .map_err(|e| e.to_string())?;
        let block = ChangeBlockOutput {
            cause: cause.clone(),
            changes: page.changes,
        };
        let (folded, complete) = cx
            .background_executor()
            .spawn(async move {
                let complete = fold.add_block_capped(block, REVIEW_CHANGE_CAP);
                (fold, complete)
            })
            .await;
        fold = folded;
        let raw = fold.raw_changes();
        this.update(cx, |this, cx| {
            this.load = LoadState::Loading { folded: raw };
            cx.notify();
        })
        .ok();
        match page.next_page_token {
            Some(next) if complete && raw < REVIEW_CHANGE_CAP => token = Some(next),
            Some(_) => {
                partial = true;
                break;
            }
            None => break,
        }
    }
    let fed = Fed {
        fold,
        commits: since.into_iter().rev().collect(),
        partial,
        moved_while_reading: false,
    };
    let mut data = fed.into_review(&session, Some(base), Some(head), cx).await;
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
    let (fork, head, since) = commits_since_fork(&session, &branch, &target, None, cx).await?;
    let Some(commit) = scope else {
        return Err("A review of one commit names the commit.".into());
    };
    let mut fold = ChangeFold::new();
    let mut partial = false;
    let mut token: Option<String> = None;
    loop {
        let limit = FEED_PAGE.min(REVIEW_CHANGE_CAP - fold.raw_changes()).max(1) as u32;
        let page = session
            .update(cx, |session, cx| {
                session.commit_changes(&commit, token.clone(), limit, cx)
            })
            .await
            .map_err(|e| e.to_string())?;
        let block = ChangeBlockOutput {
            cause: page.cause,
            changes: page.changes,
        };
        let (folded, complete) = cx
            .background_executor()
            .spawn(async move {
                let complete = fold.add_block_capped(block, REVIEW_CHANGE_CAP);
                (fold, complete)
            })
            .await;
        fold = folded;
        let raw = fold.raw_changes();
        this.update(cx, |this, cx| {
            this.load = LoadState::Loading { folded: raw };
            cx.notify();
        })
        .ok();
        match page.next_page_token {
            Some(next) if complete && raw < REVIEW_CHANGE_CAP => token = Some(next),
            Some(_) => {
                partial = true;
                break;
            }
            None => break,
        }
    }
    let fed = Fed {
        fold,
        commits: since.into_iter().rev().collect(),
        partial,
        moved_while_reading: false,
    };
    Ok(fed.into_review(&session, fork, head, cx).await)
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
    let read = session.update(cx, |session, cx| session.merge_base(source, target, cx));
    if let Ok(base) = read.await
        && (source_head == Some(base.merge_base.graph_commit_id.as_str())
            || commit == Some(base.target_commit_id.as_str()))
    {
        return Some(true);
    }
    let read = session.update(cx, |session, cx| session.commits(target, true, cx));
    let list = read.await.ok()?;
    Some(
        commit.is_some_and(|c| list.iter().any(|t| t.graph_commit_id == c))
            || source_head.is_some_and(|head| contains_merge_of(&list, head)),
    )
}

impl Render for ReviewView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let header = self.render_header(cx);
        let body = match (&self.load, &self.data) {
            (LoadState::Ready, Some(data)) if self.show_conflicts => {
                let _ = data;
                self.render_conflicts(cx)
            }
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
                if let Some(ix) = this.selected {
                    this.open_edit(ix, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &Merge, _, cx| this.merge(cx)))
            .child(header)
            .child(body)
    }
}

/// Opens (or activates) the review of `branch` against `target`. `merge` starts the merge as
/// soon as the review has read the branch head.
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
            folded,
            groups,
            partial: false,
            moved_while_reading: false,
            merged_in: false,
        };
        let shown = |query: &str| {
            let (rows, headers) = build_rows(&data, query);
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
    }

    /// A reviewed mark holds for the change as it was reviewed: changed again since, it is
    /// reviewed again.
    #[test]
    fn a_reviewed_mark_is_kept_by_the_change_and_its_last_commit() {
        let reviewed = change("p1", json!({"name": "Ada"}));
        let mut again = reviewed.clone();
        again.commits.push("c2".into());
        assert_eq!(mark_key(&reviewed), mark_key(&reviewed.clone()));
        assert_ne!(mark_key(&reviewed), mark_key(&again));
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
            let merges: Vec<String> = asked
                .lock()
                .unwrap()
                .iter()
                .filter(|request| request.contains("/branches/merge?"))
                .cloned()
                .collect();
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
            });
        }
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
        // Two people added: each revert deletes one.
        let added = |id: &str| json!({ "kind": "node", "type": { "id": "Person", "name": "Person" }, "id": id, "op": "insert", "before": null, "after": { "properties": { "slug": id, "name": "x" } } });
        let block = |id: &str, parent: &str, change: Value| json!({ "cause": { "graph_commit_id": id, "parent_commit_id": parent, "authored_branch": "feature", "authored_at": 0, "actor_id": "act-dev" }, "changes": [change] });
        let feed = json!({ "blocks": [block("f1", "m1", added("p1")), block("f2", "f1", added("p2"))], "cursor": "c", "caught_up": true });
        review_server(
            merged_in,
            Some(feed),
            "f2",
            |n| {
                let parent = if n == 0 {
                    "f2".to_owned()
                } else {
                    format!("r{n}")
                };
                (
                    200,
                    json!({ "branch": "feature", "query_name": "q", "affected_nodes": 1, "affected_edges": 0,
                        "commit": { "graph_commit_id": format!("r{}", n + 1), "graph_manifest_version": 3, "created_at": 0, "parent_commit_id": parent } }),
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
        let writes = std::sync::atomic::AtomicUsize::new(0);
        let commit = |id: &str, parent: Option<&str>| json!({ "graph_commit_id": id, "graph_manifest_version": 1, "created_at": 0, "parent_commit_id": parent, "actor_id": "act-dev" });
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
                if path.contains("/mutate") {
                    write(writes.fetch_add(1, std::sync::atomic::Ordering::SeqCst))
                } else if path.ends_with("/branches/merge") {
                    let merge = json!({ "graph_commit_id": "m3", "graph_manifest_version": 4, "created_at": 0, "parent_commit_id": "m2", "merged_parent_commit_id": took });
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
                    .header(graph_client::CONTRACT_HEADER, "0.13")
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
    /// lists. A branch main was merged into has a feed that refuses the base as a start, and
    /// whose merge block carries main's own changes: the review reads the net diff from the
    /// base instead, and the lists for the branch's commits.
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
    #[gpui::test]
    async fn a_merge_is_confirmed_after_more_of_the_source_was_merged(
        cx: &mut gpui::TestAppContext,
    ) {
        let commit = |id: &str, merged: Option<&str>| json!({ "graph_commit_id": id, "graph_manifest_version": 1, "created_at": 0, "merged_parent_commit_id": merged });
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
