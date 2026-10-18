//! The review tab (spec 008): what a branch changed since it forked from its target, folded per
//! entity, with fixes bound to the reviewed head and a merge that checks the head first.

use std::collections::HashSet;

use editor::{Editor, EditorEvent};
use gpui::{
    AnyElement, App, AsyncApp, Context, Entity, EventEmitter, FocusHandle, Focusable,
    ListAlignment, ListState, Render, SharedString, Subscription, Task, WeakEntity, Window, list,
};
use gq_language::SchemaModel;
use graph_client::api::{
    BranchMergeOutcome, BranchMergeOutput, ChangeOutput, ChangeRequest, CommitOutput,
    MergeConflictOutput,
};
use graph_client::{ReadError, WriteOutcome};
use graph_store::fixes::compose;
use graph_store::{
    ChangeFold, FoldedChange, FoldedChanges, GraphSession, HeadWatch, NetChange, REVIEW_CHANGE_CAP,
    SessionEvent, TypeCounts, commits_since, contains_merge_of, fork_point, revert_query,
    summary_sentence,
};
use serde_json::Value;
use studio_actions::review::{
    AcceptAndNext, EditChange, Merge, NextChange, PreviousChange, RevertAndNext,
};
use studio_ui::{
    ChangeCard, ChangeKind, PropertyRow, change_counts, change_group_header, group_digits,
    short_commit_id, type_swatch,
};
use studio_workspace::{Item, ItemEvent, SavedItem, Workspace, workspace_for};
use ui::prelude::*;
use ui::{Banner, ContextMenu, KeyBinding, PopoverMenu, Severity, Tooltip};

use crate::conflicts::{conflict_wording, resolution_hint};

/// How long typing in the search pauses before the cards are filtered.
const SEARCH_DELAY: std::time::Duration = std::time::Duration::from_millis(80);

/// Changes asked for per change-feed page.
const FEED_PAGE: usize = 1_000;

/// One row of the review's list.
#[derive(Debug, Clone, Copy)]
enum Row {
    Group(usize),
    Card(usize),
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

#[derive(Debug, Clone)]
struct CardState {
    reviewed: bool,
    fix: FixState,
    /// Why Revert is unavailable, when it is.
    revert_blocked: Option<String>,
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
        /// `None` while Studio reads the target's history to confirm.
        verified: Option<bool>,
    },
    Conflicts(Vec<MergeConflictOutput>),
    Refused(String),
    Unknown {
        reason: String,
        /// What reading the target's history found, once read.
        landed: Option<bool>,
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
    /// Each change's search text, lowercase, made when it was folded: a search goes through
    /// these, not through every value of every change on each key.
    search: Vec<String>,
    counts: Vec<TypeCounts>,
    partial: bool,
    /// The feed went past the reviewed head while reading: the branch moved.
    moved_while_reading: bool,
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
    cards: Vec<CardState>,
    rows: Vec<Row>,
    selected: Option<usize>,
    list_state: ListState,
    /// The head fixes and the merge bind to: the reviewed head, then each fix's own commit.
    bound_head: Option<String>,
    /// Reviewed marks, by entity key, kept across Refresh.
    reviewed_keys: HashSet<String>,
    merge: MergeState,
    merge_when_loaded: bool,
    show_conflicts: bool,
    /// Someone else wrote to the branch after this review was read.
    newer: bool,
    fixes_running: usize,
    /// The session saw the branch's head move while a fix or the merge was out; compared once
    /// they are back, since each moves the bound head itself.
    head_moved_while_writing: bool,
    /// The session asks after the reviewed branch's head while the review is open.
    _head_watch: HeadWatch,
    _load_task: Option<Task<()>>,
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
            selected: None,
            list_state: ListState::new(0, ListAlignment::Top, px(800.)),
            bound_head: None,
            reviewed_keys: HashSet::new(),
            merge: MergeState::Idle,
            merge_when_loaded: false,
            show_conflicts: false,
            newer: false,
            fixes_running: 0,
            head_moved_while_writing: false,
            _head_watch: head_watch,
            _load_task: None,
            _merge_task: None,
            _search_task: None,
            _fix_tasks: Vec::new(),
            _subscriptions: vec![subscription, search_subscription],
        };
        view.reload(cx);
        view
    }

    pub fn branch(&self) -> &str {
        &self.branch
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
                self.reviewed_keys.insert(change.key());
            } else {
                self.reviewed_keys.remove(&change.key());
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
                let schema = self.session.read(cx).schema_model();
                self.cards = data
                    .folded
                    .changes
                    .iter()
                    .map(|change| CardState {
                        reviewed: self.reviewed_keys.contains(&change.key()),
                        fix: FixState::Idle,
                        revert_blocked: match &schema {
                            Some(schema) => revert_query(schema, change).err().map(|e| e.0),
                            None => None,
                        },
                    })
                    .collect();
                self.rows = build_rows(&data, &self.search_text(cx));
                self.list_state.reset(self.rows.len());
                self.selected = self.rows.iter().find_map(|row| match row {
                    Row::Card(ix) => Some(*ix),
                    Row::Group(_) => None,
                });
                self.bound_head = data.head.clone();
                self.newer = data.moved_while_reading;
                self.data = Some(data);
                self.load = LoadState::Ready;
                if std::mem::take(&mut self.merge_when_loaded) {
                    self.merge(cx);
                }
            }
            Err(error) => self.load = LoadState::Failed(error.into()),
        }
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    fn change_count(&self) -> usize {
        self.data.as_ref().map_or(0, |d| d.folded.changes.len())
    }

    fn row_of_change(&self, change: usize) -> Option<usize> {
        self.rows
            .iter()
            .position(|row| matches!(row, Row::Card(ix) if *ix == change))
    }

    fn select(&mut self, change: usize, cx: &mut Context<Self>) {
        if change < self.change_count() {
            self.selected = Some(change);
            if let Some(row) = self.row_of_change(change) {
                self.list_state.scroll_to_reveal_item(row);
            }
            cx.notify();
        }
    }

    /// The cards the search leaves, in list order.
    fn visible_cards(&self) -> Vec<usize> {
        self.rows
            .iter()
            .filter_map(|row| match row {
                Row::Card(ix) => Some(*ix),
                Row::Group(_) => None,
            })
            .collect()
    }

    fn step(&mut self, forward: bool, cx: &mut Context<Self>) {
        let visible = self.visible_cards();
        let Some(&first) = visible.first() else {
            return;
        };
        let next = match self
            .selected
            .and_then(|s| visible.iter().position(|&ix| ix == s))
        {
            Some(at) if forward => visible[(at + 1).min(visible.len() - 1)],
            Some(at) => visible[at.saturating_sub(1)],
            None => first,
        };
        self.select(next, cx);
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

    /// Shows only the cards matching the search, keeping the selection when it still shows.
    fn apply_search(&mut self, cx: &mut Context<Self>) {
        let Some(data) = &self.data else { return };
        self.rows = build_rows(data, &self.search_text(cx));
        self.list_state.reset(self.rows.len());
        let visible = self.visible_cards();
        if !self.selected.is_some_and(|s| visible.contains(&s)) {
            self.selected = visible.first().copied();
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

    fn accept_and_next(&mut self, _: &AcceptAndNext, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.selected {
            if let Some(card) = self.cards.get_mut(ix) {
                card.reviewed = true;
            }
            self.select_next(&NextChange, window, cx);
        }
    }

    fn revert_and_next(&mut self, _: &RevertAndNext, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.selected {
            self.revert(ix, cx);
            self.select_next(&NextChange, window, cx);
        }
    }

    fn toggle_reviewed(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(card) = self.cards.get_mut(ix) {
            card.reviewed = !card.reviewed;
            cx.notify();
        }
    }

    /// Restores a change's before image on the branch, bound to the head this review holds.
    /// Never retried; the card shows the outcome.
    fn revert(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(data) = &self.data else { return };
        let Some(change) = data.folded.changes.get(ix) else {
            return;
        };
        let Some(card) = self.cards.get(ix) else {
            return;
        };
        if card.fix == FixState::Running || matches!(card.fix, FixState::Reverted { .. }) {
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
        let description = format!("Revert {} {}", change.type_name, change.id);
        let if_commit = self.bound_head.clone();
        self.cards[ix].fix = FixState::Running;
        self.fixes_running += 1;
        let task = self.session.update(cx, |session, cx| {
            session.write_request(request, if_commit, description, cx)
        });
        self._fix_tasks.push(cx.spawn(async move |this, cx| {
            let outcome = task.await;
            this.update(cx, |this, cx| this.finish_fix(ix, outcome, false, cx))
                .ok();
        }));
        cx.notify();
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

    fn open_edit(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(values) = self.edit_values(ix, cx) else {
            return;
        };
        let Some(change) = self.data.as_ref().and_then(|d| d.folded.changes.get(ix)) else {
            return;
        };
        let title = format!("{} {}", change.type_name, change.id);
        let branch = self.branch.clone();
        let view = cx.entity().downgrade();
        let Some(workspace) = workspace_for(window) else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, move |window, cx| {
                crate::edit::EditModal::new(view, ix, title, branch, values, window, cx)
            })
        });
    }

    /// Writes edited fields on the reviewed branch, each bound to the value the review showed,
    /// in one mutation bound to the head this review holds. Never retried.
    pub(crate) fn save_edit(
        &mut self,
        ix: usize,
        changes: Vec<(String, Value, Value)>,
        cx: &mut Context<Self>,
    ) {
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
        let description = format!("Edit {} {}", change.type_name, change.id);
        let if_commit = self.bound_head.clone();
        self.cards[ix].fix = FixState::Saving;
        self.fixes_running += 1;
        let task = self.session.update(cx, |session, cx| {
            session.write_request(request, if_commit, description, cx)
        });
        self._fix_tasks.push(cx.spawn(async move |this, cx| {
            let outcome = task.await;
            this.update(cx, |this, cx| this.finish_fix(ix, outcome, true, cx))
                .ok();
        }));
        cx.notify();
    }

    fn finish_fix(
        &mut self,
        ix: usize,
        outcome: WriteOutcome<ChangeOutput>,
        edited: bool,
        cx: &mut Context<Self>,
    ) {
        self.fixes_running = self.fixes_running.saturating_sub(1);
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
        if let Some(card) = self.cards.get_mut(ix) {
            if matches!(fix, FixState::Reverted { .. }) {
                card.reviewed = true;
            }
            card.fix = fix;
        }
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

    /// Merges the branch into the target after checking the branch still sits at the head this
    /// review holds. The outcome is shown, then confirmed against the target's history.
    pub fn merge(&mut self, cx: &mut Context<Self>) {
        match self.load {
            LoadState::Loading { .. } => {
                self.merge_when_loaded = true;
                return;
            }
            LoadState::Failed(_) => return,
            LoadState::Ready => {}
        }
        if matches!(self.merge, MergeState::Checking | MergeState::Merging) {
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
                session.update(cx, |s, cx| s.merge(branch.clone(), target.clone(), cx))
            }) else {
                return;
            };
            let outcome = task.await;
            let state = match outcome {
                WriteOutcome::Committed(output) => {
                    let BranchMergeOutput {
                        outcome, commit, ..
                    } = output;
                    let commit = commit.map(|c| c.graph_commit_id);
                    this.update(cx, |this, cx| {
                        this.merge = MergeState::Merged {
                            outcome,
                            commit: commit.clone(),
                            verified: None,
                        };
                        cx.notify();
                    })
                    .ok();
                    let verified = verify_merge(
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
                        verified: Some(verified),
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
                            landed: None,
                        };
                        cx.notify();
                    })
                    .ok();
                    // Settle by reading history, never by merging again (constitution III).
                    let landed =
                        merge_landed(&session, &branch, &target, bound.as_deref(), cx).await;
                    MergeState::Unknown { reason, landed }
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
        let mergeable = matches!(self.load, LoadState::Ready)
            && !matches!(self.merge, MergeState::Checking | MergeState::Merging);
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
                "omnigraph branch merge {} --into {}",
                self.branch, self.target
            );
            move |_, cx| {
                Tooltip::with_meta(
                    "Merge after checking the branch has not moved",
                    None,
                    cli.clone(),
                    cx,
                )
            }
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
                        .gap_1p5()
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
            .px_4()
            .pt_3()
            .pb_2()
            .gap_2()
            .border_b_1()
            .border_color(border)
            .child(
                h_flex()
                    .gap_2()
                    .child(Icon::new(IconName::GitBranch).color(Color::Muted))
                    .child(
                        Headline::new(format!("{} → {}", self.branch, self.target))
                            .size(HeadlineSize::Small),
                    )
                    .child(div().flex_1())
                    .child(refresh)
                    .child(merge_button),
            )
            .child(h_flex().gap_2().child(scope_menu).child(search))
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
                };
                let check = match verified {
                    None => format!(" Checking {}'s history…", self.target),
                    Some(true) => format!(" Confirmed in {}'s history.", self.target),
                    Some(false) => format!(
                        " Not found in {}'s history yet; open History to check.",
                        self.target
                    ),
                };
                (
                    if *verified == Some(false) {
                        Severity::Warning
                    } else {
                        Severity::Success
                    },
                    format!("{what}{check}"),
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
                    None => format!(
                        "The merge outcome is unknown ({reason}). Studio did not retry; reading {}'s history…",
                        self.target
                    ),
                    Some(true) => format!(
                        "The merge response was lost ({reason}), but {}'s history shows the merge landed.",
                        self.target
                    ),
                    Some(false) => format!(
                        "The merge response was lost ({reason}) and {}'s history does not show it. Studio did not retry; merge again when ready.",
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
            .p_4()
            .gap_2()
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
                    .gap_3()
                    .px_3()
                    .py_2()
                    .rounded_md()
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
                                    .gap_2()
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
            Some(Row::Group(ix)) => {
                let count = &data.counts[ix];
                div()
                    .px_4()
                    .child(change_group_header(
                        &count.type_name,
                        count.is_edge,
                        count.added,
                        count.changed,
                        count.removed,
                    ))
                    .into_any_element()
            }
            Some(Row::Card(ix)) => {
                let change = &data.folded.changes[ix];
                let state = self.cards[ix].clone();
                let schema = self.session.read(cx).schema_model();
                let mut card = change_card(change, schema.as_deref(), ("change", ix))
                    .selected(self.selected == Some(ix))
                    .hollow(state.reviewed);
                card = match &state.fix {
                    FixState::Idle => card,
                    FixState::Running => card.status("Reverting…", Color::Muted),
                    FixState::Saving => card.status("Saving…", Color::Muted),
                    FixState::Reverted { commit } => card.status(
                        format!("Reverted in {}", short_commit_id(commit)),
                        Color::Success,
                    ),
                    FixState::Edited { commit } => card.status(
                        format!("Edited in {}", short_commit_id(commit)),
                        Color::Success,
                    ),
                    FixState::Unchanged => card.status(
                        "Not written: the value changed since this review. Refresh.",
                        Color::Warning,
                    ),
                    FixState::Moved => card.status(
                        "Not written: the branch moved. Refresh, then try again.",
                        Color::Warning,
                    ),
                    FixState::Refused(message) => {
                        card.status(format!("Refused: {message}"), Color::Error)
                    }
                    FixState::Unknown => card.status(
                        "Outcome unknown. Check History before trying again.",
                        Color::Warning,
                    ),
                };
                let focus = self.focus_handle.clone();
                let reviewed_label = if state.reviewed {
                    "Reviewed"
                } else {
                    "Mark reviewed"
                };
                let review_button = Button::new(("reviewed", ix), reviewed_label)
                    .label_size(LabelSize::XSmall)
                    .style(ButtonStyle::Subtle)
                    .toggle_state(state.reviewed)
                    .when(state.reviewed, |b| {
                        b.start_icon(Icon::new(IconName::Check).size(IconSize::XSmall))
                    })
                    .tooltip({
                        let focus = focus.clone();
                        move |_, cx| {
                            Tooltip::for_action_in(
                                "Mark reviewed and go to the next change",
                                &AcceptAndNext,
                                &focus,
                                cx,
                            )
                        }
                    })
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_reviewed(ix, cx)));
                let blocked = state.revert_blocked.clone();
                // After a fix the card shows the review as read; the next one waits for Refresh.
                let done = matches!(
                    state.fix,
                    FixState::Running
                        | FixState::Saving
                        | FixState::Reverted { .. }
                        | FixState::Edited { .. }
                );
                let editable = self.edit_values(ix, cx).is_some();
                let edit_button = editable.then(|| {
                    Button::new(("edit", ix), "Edit")
                        .label_size(LabelSize::XSmall)
                        .style(ButtonStyle::Subtle)
                        .start_icon(Icon::new(IconName::Pencil).size(IconSize::XSmall))
                        .disabled(done)
                        .tooltip({
                            let focus = focus.clone();
                            let branch = self.branch.clone();
                            move |_, cx| {
                                Tooltip::for_action_in(
                                    format!("Change what {branch} holds now"),
                                    &EditChange,
                                    &focus,
                                    cx,
                                )
                            }
                        })
                        .on_click(
                            cx.listener(move |this, _, window, cx| this.open_edit(ix, window, cx)),
                        )
                });
                let branch = self.branch.clone();
                let revert_button = Button::new(("revert", ix), "Revert")
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
                    .on_click(cx.listener(move |this, _, _, cx| this.revert(ix, cx)));
                div()
                    .px_4()
                    .py_1()
                    .child(
                        card.action(review_button)
                            .when_some(edit_button, |card, button| card.action(button))
                            .action(revert_button)
                            .on_click(cx.listener(move |this, _, _, cx| this.select(ix, cx))),
                    )
                    .into_any_element()
            }
            None => div().into_any_element(),
        }
    }
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

/// Whether a change's id, type or any before or after value contains `query` (lowercase).
/// What a search finds a change by, lowercase: its id, its type and each value before and
/// after, one per line, so a search never matches across two of them.
fn search_text_of(change: &FoldedChange) -> String {
    let mut text = format!("{}\n{}", change.id, change.type_name);
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

#[cfg(test)]
fn change_matches(change: &FoldedChange, query: &str) -> bool {
    query.is_empty() || search_text_of(change).contains(query)
}

fn build_rows(data: &ReviewData, query: &str) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut group = None;
    let mut group_index = 0;
    for (ix, change) in data.folded.changes.iter().enumerate() {
        if !query.is_empty() && !data.search.get(ix).is_some_and(|text| text.contains(query)) {
            continue;
        }
        let key = (change.is_edge(), change.type_id.as_str());
        if group != Some(key) {
            if group.is_some() {
                group_index += 1;
            }
            group = Some(key);
            rows.push(Row::Group(group_index));
        }
        rows.push(Row::Card(ix));
    }
    rows
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
                search: Vec::new(),
                counts: Vec::new(),
                partial: false,
                moved_while_reading: false,
            });
        }
        let start = format!("after:{fork}");
        match fold_feed(&session, &branch, start, &head, None, &this, cx).await {
            Ok(fed) => return Ok(fed.into_review(Some(fork), Some(head), cx).await),
            Err(FeedError::Refused) => {}
            Err(FeedError::Failed(error)) => return Err(error),
        }
    }
    read_review_by_histories(session, branch, target, scope, this, cx).await
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
        fork: Option<String>,
        head: Option<String>,
        cx: &mut AsyncApp,
    ) -> ReviewData {
        let fold = self.fold;
        let (folded, search) = cx
            .background_executor()
            .spawn(async move {
                let folded = fold.finish();
                let search = folded.changes.iter().map(search_text_of).collect();
                (folded, search)
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
            search,
            counts,
            partial: self.partial,
            moved_while_reading: self.moved_while_reading,
        }
    }
}

enum FeedError {
    /// The feed refused the start commit.
    Refused,
    Failed(String),
}

/// Folds `branch`'s change feed from `start` (`after:<fork>`, or `beginning`) up to `head`, the
/// change cap, or a block the review did not read the branch at: one outside `known`, when
/// given, else one after `head`.
async fn fold_feed(
    session: &Entity<GraphSession>,
    branch: &str,
    start: String,
    head: &str,
    known: Option<&HashSet<String>>,
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
            let outside = match known {
                Some(known) => !known.contains(id),
                None => reached_head && commits.last().map(|c: &ReviewCommit| &c.id) != Some(id),
            };
            if outside {
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

/// Reads both histories, finds the fork, and folds the branch's change feed from there, up to
/// the reviewed head and the change cap; or one commit's own changes when `scope` names it.
async fn read_review_by_histories(
    session: Entity<GraphSession>,
    branch: String,
    target: String,
    scope: Option<String>,
    this: WeakEntity<ReviewView>,
    cx: &mut AsyncApp,
) -> Result<ReviewData, String> {
    // Read now, not from the session's shared lists: a review shows what the branches hold.
    let branch_commits = session
        .update(cx, |session, cx| session.commits(&branch, true, cx))
        .await
        .map_err(|e| e.to_string())?;
    let target_commits = session
        .update(cx, |session, cx| session.commits(&target, true, cx))
        .await
        .map_err(|e| e.to_string())?;
    let fork = fork_point(&branch_commits, &target_commits).map(|f| f.graph_commit_id.clone());
    let head = branch_commits.first().map(|h| h.graph_commit_id.clone());
    let since: Vec<ReviewCommit> = commits_since(&branch_commits, fork.as_deref())
        .iter()
        .map(ReviewCommit::from)
        .collect();
    if let Some(commit) = &scope {
        // One commit: its own changes, page by page, up to the cap.
        let mut fold = ChangeFold::new();
        let mut partial = false;
        let mut token: Option<String> = None;
        loop {
            let limit = FEED_PAGE.min(REVIEW_CHANGE_CAP - fold.raw_changes()).max(1) as u32;
            let page = session
                .update(cx, |session, cx| {
                    session.commit_changes(commit, token.clone(), limit, cx)
                })
                .await
                .map_err(|e| e.to_string())?;
            let block = graph_client::api::ChangeBlockOutput {
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
        let mut commits = since;
        commits.reverse();
        let fed = Fed {
            fold,
            commits,
            partial,
            moved_while_reading: false,
        };
        return Ok(fed.into_review(fork, head, cx).await);
    }
    let (Some(head_id), false) = (head.clone(), since.is_empty()) else {
        let fed = Fed {
            fold: ChangeFold::new(),
            commits: Vec::new(),
            partial: false,
            moved_while_reading: false,
        };
        return Ok(fed.into_review(fork, head, cx).await);
    };
    let known: HashSet<String> = since.iter().map(|c| c.id.clone()).collect();
    // A branch sharing no history with the target is read from its beginning.
    let start = match &fork {
        Some(fork) => format!("after:{fork}"),
        None => "beginning".to_owned(),
    };
    let fed = fold_feed(&session, &branch, start, &head_id, Some(&known), &this, cx).await;
    let mut fed = fed.map_err(|error| match error {
        FeedError::Refused => "The change feed refused the fork as a start.".to_owned(),
        FeedError::Failed(error) => error,
    })?;
    // The commit list names every commit since the fork, also those whose block the cap cut.
    fed.commits = since.into_iter().rev().collect();
    Ok(fed.into_review(fork, head, cx).await)
}

/// Whether the target holds the merge just reported.
async fn verify_merge(
    session: &Entity<GraphSession>,
    source: &str,
    target: &str,
    commit: Option<&str>,
    source_head: Option<&str>,
    cx: &mut AsyncApp,
) -> bool {
    merge_shown(session, source, target, commit, source_head, cx)
        .await
        .unwrap_or(false)
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
            .py_2()
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
pub fn open_review(
    workspace: &mut Workspace,
    branch: String,
    target: String,
    merge: bool,
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
    let view = workspace.open_item::<ReviewView>(key, window, cx, |window, cx| {
        ReviewView::new(session, branch, target, window, cx)
    });
    if merge && let Some(view) = view {
        view.update(cx, |view, cx| view.merge(cx));
    }
}

#[cfg(test)]
mod tests {
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
        assert!(change_matches(&c, ""));
        assert!(change_matches(&c, "000001"));
        assert!(change_matches(&c, "person"));
        assert!(change_matches(&c, "dijkstra"));
        assert!(change_matches(&c, "1985"));
        assert!(!change_matches(&c, "lovelace"));
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
        let commit = |id: &str, parent: Option<&str>| json!({ "graph_commit_id": id, "graph_manifest_version": 1, "created_at": 0, "parent_commit_id": parent, "actor_id": "act-dev" });
        let person = |id: &str, op: &str| json!({ "kind": "node", "type": { "id": "Person", "name": "Person" }, "id": id, "op": op, "before": { "properties": { "slug": id } }, "after": { "properties": { "slug": id, "name": "x" } } });
        let block = |id: &str, parent: &str, change: Value| json!({ "cause": { "graph_commit_id": id, "parent_commit_id": parent, "authored_branch": "feature", "authored_at": 0, "actor_id": "act-dev" }, "changes": [change] });
        let base_id = if merged_in { "m2" } else { "m1" };
        let base = json!({ "merge_base": commit(base_id, Some("m1")), "source": "feature", "source_commit_id": "f2", "target": "main", "target_commit_id": "m2" });
        let feed = json!({ "blocks": [block("f1", "m1", person("p1", "insert")), block("f2", "f1", person("p2", "update"))], "cursor": "c", "caught_up": true });
        let feature = json!({ "commits": [commit("f2", Some("f1")), commit("f1", Some("m1")), commit("m1", None)] });
        let main = json!({ "commits": [commit("m2", Some("m1")), commit("m1", None)] });
        fake_server(
            move |path, query| {
                if path.ends_with("/schema") {
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

    /// A session on a server answering each request's path and query with `answer`; every
    /// request is recorded.
    fn fake_server(
        answer: impl Fn(&str, &str) -> (u16, Value) + Send + Sync + 'static,
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
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            let query = request.uri().query().unwrap_or_default().to_owned();
            seen.lock().unwrap().push(format!("{path}?{query}"));
            let (status, body) = answer(&path, &query);
            async move {
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
    /// lists; a feed that refuses the base as a start falls back to the lists.
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
        assert_eq!(data.fork.as_deref(), Some("m1"), "the first-parent fork");
        assert_eq!(data.head.as_deref(), Some("f2"));
        assert_eq!(data.folded.changes.len(), 2);
        let asked = asked.lock().unwrap().clone();
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
            move |path, _| {
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
        assert!(
            verify_merge(
                &session,
                "feature",
                "main",
                Some("m3"),
                Some("f2"),
                &mut async_cx
            )
            .await
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
    }
}
