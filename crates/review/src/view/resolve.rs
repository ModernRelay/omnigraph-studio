//! A conflict's three sides and a decision (spec 039): what the fork, the target now and this
//! branch hold of each conflicting entity, who set the target's, and the writes that resolve
//! it ("Keep main's", "Keep this branch's", "Edit…", and for too many relationships, removing
//! one on its side).
//!
//! Omnigraph's merge conflicts name the kind, the type, the entity and a message, never the
//! values (E53). Studio reads them itself: the target's and the branch's images at their heads,
//! named by commit (constitution II), the fork's from the review's own changes, and who set the
//! target's from the target's change feed since the fork, filtered by type (the feed has no
//! filter by entity, E62). A decision is an ordinary write: the target's values onto the branch,
//! bound to the head the review's fixes bind to, or the branch's onto the target, bound to the
//! target head the sides were read at, once confirmed. A merge that takes a side (E61) would
//! make the second one merge instead of a write to the target.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use gpui::{AnyElement, AsyncApp, Context, Entity, Task, Window};
use gq_language::SchemaModel;
use gq_language::builder::{self, EdgesOf};
use gq_language::input::same_value;
use graph_client::WriteOutcome;
use graph_client::api::{
    ChangeOutput, ChangeRequest, EntityKind, MergeConflictKind, MergeConflictOutput,
};
use graph_store::fixes::{SideImage, take_side};
use graph_store::{AccessAction, AccessScope, GraphSession, NodeNames};
use serde_json::{Map, Value};
use studio_actions::review::{EditChange, KeepSource, KeepTarget, ShowConflicts};
use studio_ui::ds::Tipped;
use studio_ui::ds::{radii, size, space};
use studio_ui::{short_commit_id, type_swatch};
use ui::prelude::*;

use super::{BRANCH_DELETED, Fix, FixState, MERGE_RUNNING, ReviewView};
use crate::conflicts::conflict_wording;

/// The conflicts whose sides are read, at most: each type's ids are read in one query at each
/// head, so this bounds a read's size, not its count. Past it a card says what the server said
/// (constitution IX).
pub(super) const SIDES_CAP: usize = 200;
/// Pages of the target's change feed read for who set its side, at most, each of up to
/// [`super::FEED_PAGE`] changes of the conflicts' types.
const WHO_PAGES: usize = 4;
/// Merged branch heads read by id for the branch and author a merge brought a value from, at
/// most.
const MERGED_FROM_READS: usize = 20;
/// Relationships of one type from one node read for a `@card` conflict, at most, on each side.
const CARD_EDGES: u32 = 50;

static NEXT_LIST: AtomicU64 = AtomicU64::new(1);

/// A new conflicts list's identity: the sides read are for one list, and a dry run or a merge
/// that answers again makes another.
pub(super) fn next_list_id() -> u64 {
    NEXT_LIST.fetch_add(1, Ordering::Relaxed)
}

/// What one side holds of an entity.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Held {
    /// Its properties as read, and for a relationship its two ends.
    Holds(SideImage, Option<(String, String)>),
    /// Read, and not there: never added on that side, or deleted.
    Absent,
    /// Not read: past [`SIDES_CAP`], a review cut short, or a read that failed.
    Unread,
}

impl Held {
    fn image(&self) -> Option<&SideImage> {
        match self {
            Held::Holds(image, _) => Some(image),
            _ => None,
        }
    }

    fn known(&self) -> bool {
        !matches!(self, Held::Unread)
    }
}

/// Who last changed an entity on one side since the fork.
#[derive(Debug, Clone, PartialEq, Default)]
pub(super) struct SetBy {
    pub(super) actor: Option<String>,
    /// The branch the change was made on, when another than the side's own: a merge brought it.
    pub(super) from: Option<String>,
    /// Who merged it in, when a merge brought it.
    pub(super) merged_by: Option<String>,
    /// Unix microseconds; 0 when not known.
    pub(super) at: i64,
}

/// One relationship a `@card` conflict counts, and where it is.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct CardEdge {
    pub(super) id: String,
    pub(super) to: String,
    pub(super) on_target: bool,
    pub(super) on_source: bool,
    /// Who added it on the target, when only the target holds it.
    pub(super) by: Option<SetBy>,
}

/// What a conflict card shows beyond the server's words.
#[derive(Debug, Clone, PartialEq)]
// One a conflict, at most `SIDES_CAP` of them: the variants' sizes do not matter.
#[allow(clippy::large_enum_variant)]
pub(super) enum Sides {
    /// A node or relationship the two sides disagree on: its three images, and who set each
    /// side's.
    Entity {
        base: Held,
        target: Held,
        source: Held,
        target_by: Option<SetBy>,
        source_by: Option<SetBy>,
    },
    /// More relationships from one node than the schema allows: those each side holds.
    Card {
        edge: String,
        from: String,
        edges: Vec<CardEdge>,
    },
    /// Nothing more to show than the server's words: a value constraint, or a conflict naming
    /// no entity Studio can read.
    Words,
}

/// The sides read for one conflicts list.
pub(super) struct SidesRead {
    /// The target's head the sides were read at, which a write to the target binds to; each
    /// such write's commit moves it on.
    pub(super) target_head: String,
    /// By the conflict's index in the list, the first [`SIDES_CAP`].
    pub(super) each: Vec<Sides>,
    /// The other ends of the relationships shown, and the conflicting nodes' names.
    pub(super) names: NodeNames,
    /// Whether the target's feed was read through for who set its side.
    pub(super) who_whole: bool,
}

pub(super) enum SidesState {
    Unread,
    Reading,
    Read(SidesRead),
    Failed(String),
}

/// Which way a decision resolves a conflict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Choice {
    /// The target's version, written onto the branch.
    Target,
    /// This branch's version, written onto the target.
    Source,
    /// The branch's version edited.
    Edit,
    /// One relationship removed on one side.
    Remove { edge: String, on_target: bool },
}

/// A decision under way or done.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Decision {
    Writing(Choice),
    Wrote(Choice, String),
    /// Bound to values that changed since, so it matched nothing.
    Unchanged(Choice),
    /// The branch it writes to moved.
    Moved(Choice),
    Refused(Choice, String),
    NotSent(Choice, String),
    Unknown(Choice),
    /// Not offered for this conflict, with why.
    Unavailable(String),
}

/// A conflict list's resolution state, kept by the review.
pub(super) struct Resolve {
    /// The conflicts list the sides are for ([`super::Conflicts::id`]).
    pub(super) of: Option<u64>,
    pub(super) state: SidesState,
    /// The conflict the keys act on, by its index in the list.
    pub(super) selected: usize,
    /// Each conflict's decision, by its index in the list, for the list it was made in.
    pub(super) decisions: HashMap<usize, Decision>,
    /// The conflict each write to the branch answers for, by the key its fix was sent with.
    pub(super) pending: HashMap<String, usize>,
    /// A write to the target is out: they go one at a time.
    pub(super) target_out: bool,
    pub(super) _task: Option<Task<()>>,
    pub(super) _target_task: Option<Task<()>>,
}

impl Default for Resolve {
    fn default() -> Self {
        Self {
            of: None,
            state: SidesState::Unread,
            selected: 0,
            decisions: HashMap::new(),
            pending: HashMap::new(),
            target_out: false,
            _task: None,
            _target_task: None,
        }
    }
}

/// The key a decision's write to the branch is sent with when the review holds no change of
/// the entity, so its outcome finds the conflict and no card.
fn conflict_key(list: u64, ix: usize) -> String {
    format!("conflict:{list}:{ix}")
}

/// The node a `@card` conflict counts the relationships of, from the server's words: the
/// conflict names no entity (Omnigraph `validate/kernels.rs` `cardinality_violation`), and its
/// message is "@card violation on edge <Edge>: source '<id>' has <n> edges (<bound> <limit>)".
pub(super) fn card_source(message: &str) -> Option<&str> {
    let rest = message.split_once("source '")?.1;
    let (id, after) = rest.rsplit_once("' has ")?;
    after.contains("edges").then_some(id)
}

/// The rows a conflict card compares: each property, in the schema's order, whose values the
/// sides read do not all agree on; the fork's, the target's and the branch's, `None` where a
/// side does not hold the entity or did not read it. With them, how many properties agree.
pub(super) fn compared_rows(
    order: &[String],
    base: &Held,
    target: &Held,
    source: &Held,
) -> (Vec<ComparedRow>, usize) {
    let sides = [base, target, source];
    let mut names: Vec<&String> = order.iter().collect();
    // A property the schema no longer lists, which an image still holds, comes last.
    let known: HashSet<&String> = order.iter().collect();
    let mut extra: Vec<&String> = sides
        .iter()
        .filter_map(|side| side.image())
        .flat_map(|image| image.keys())
        .filter(|name| !known.contains(name))
        .collect();
    extra.sort();
    extra.dedup();
    names.extend(extra);
    let mut rows = Vec::new();
    let mut agree = 0;
    for name in names {
        let values: [Option<Value>; 3] =
            sides.map(|side| side.image().and_then(|image| image.get(name)).cloned());
        // Compared among the sides that hold the entity and read the property.
        let held: Vec<&Value> = sides
            .iter()
            .zip(&values)
            .filter(|(side, _)| side.image().is_some())
            .filter_map(|(_, value)| value.as_ref())
            .collect();
        if held.is_empty() {
            continue;
        }
        let alike = held.windows(2).all(|pair| same_value(pair[0], pair[1]));
        // A side without the entity disagrees with every side holding one: each value of the
        // others is shown beside its absence.
        let absent = sides.iter().any(|side| matches!(side, Held::Absent));
        if alike && !absent {
            agree += 1;
        } else if !(alike && absent && held.iter().all(|value| value.is_null())) {
            rows.push((name.clone(), values));
        }
    }
    (rows, agree)
}

/// A property's name and its values at the fork, the target and the branch.
pub(super) type ComparedRow = (String, [Option<Value>; 3]);

/// An entity as one read returned it: its properties, and a relationship's two ends.
type ReadImage = (SideImage, Option<(String, String)>);

/// What each head holds of each conflicting entity, by kind, type and id: `None` read and not
/// there.
type HeldAt = HashMap<(bool, String, String), Option<ReadImage>>;

/// What the review knows of a conflicting entity from its own changes, worked out on the main
/// thread before the sides are read.
#[derive(Clone)]
struct Known {
    /// The fork's image: the change's before image, or the branch head's when the branch did
    /// not change it; `None` when the review cannot tell.
    base: Option<Held>,
    /// Who last changed it on the branch.
    source_by: Option<SetBy>,
}

/// Everything the read of the sides needs, taken on the main thread.
struct Plan {
    branch: String,
    target: String,
    conflicts: Vec<MergeConflictOutput>,
    known: Vec<Known>,
    schema: Arc<SchemaModel>,
    /// The fork the review read, whose feed on the target is read for who set its side.
    fork: Option<String>,
    /// The head the review's fixes bind to.
    source_head: Option<String>,
}

impl ReviewView {
    /// The sides of the conflicts shown are read once the frame is out, when they are for
    /// another list than the one held: a dry run or a merge answered again, or none read yet.
    pub(super) fn sides_owed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(list) = self.conflicts().map(|conflicts| conflicts.id) else {
            return;
        };
        if self.resolve.of == Some(list) || self.data.is_none() {
            return;
        }
        self.resolve.of = Some(list);
        self.resolve.state = SidesState::Reading;
        self.resolve.decisions.clear();
        self.resolve.pending.clear();
        let count = self.conflicts().map_or(0, |conflicts| conflicts.len());
        self.resolve.selected = self.resolve.selected.min(count.saturating_sub(1));
        cx.defer_in(window, |this, _, cx| this.read_sides(cx));
    }

    /// Reads the sides of the conflicts shown: the target's head, then each type's conflicting
    /// entities at both heads, the relationships a `@card` conflict counts, the names of what
    /// they join, and who set the target's side.
    fn read_sides(&mut self, cx: &mut Context<Self>) {
        let Some(conflicts) = self.conflicts() else {
            return;
        };
        let list = conflicts.id;
        let conflicts: Vec<MergeConflictOutput> =
            conflicts.iter().take(SIDES_CAP).cloned().collect();
        let Some(schema) = self.session.read(cx).schema_model() else {
            self.resolve.state = SidesState::Failed("The schema has not loaded yet.".into());
            cx.notify();
            return;
        };
        let known = conflicts
            .iter()
            .map(|conflict| self.known_of(conflict))
            .collect();
        let plan = Plan {
            branch: self.branch.clone(),
            target: self.target.clone(),
            conflicts,
            known,
            schema,
            fork: self.data.as_ref().and_then(|data| data.fork.clone()),
            source_head: self.bound_head.clone(),
        };
        // What the credential may do with the target: a decision that writes there is offered
        // only where it may.
        self.session.update(cx, |session, cx| {
            session.read_access(AccessScope::branch(&self.target), cx)
        });
        let session = self.session.clone();
        self.resolve._task = Some(cx.spawn(async move |this, cx| {
            let read = read_sides(&session, plan, cx).await;
            this.update(cx, |this, cx| {
                if this.resolve.of != Some(list) {
                    return;
                }
                this.resolve.state = match read {
                    Ok(read) => SidesState::Read(read),
                    Err(error) => SidesState::Failed(error),
                };
                cx.notify();
            })
            .ok();
        }));
    }

    /// What the review's own changes say of a conflict's entity.
    fn known_of(&self, conflict: &MergeConflictOutput) -> Known {
        let (Some(data), Some(id)) = (self.data.as_ref(), conflict.entity_id.as_deref()) else {
            return Known {
                base: Some(Held::Unread),
                source_by: None,
            };
        };
        match data.change_to(&conflict.type_name, id) {
            Some(ix) => {
                let change = &data.folded.changes[ix];
                let base = match &change.before {
                    Some(image) => Held::Holds(image.properties.clone(), None),
                    None => Held::Absent,
                };
                let source_by = change.commits.last().and_then(|last| {
                    let commit = data.commits.iter().find(|commit| commit.id == *last)?;
                    Some(SetBy {
                        actor: commit.actor_id.clone(),
                        at: commit.created_at,
                        ..SetBy::default()
                    })
                });
                Known {
                    base: Some(base),
                    source_by,
                }
            }
            // The branch did not change it since the fork, so the fork holds what the branch
            // does: read with the branch's side. Not so from a review cut short or of one
            // commit, which may not show the change.
            None if !data.partial && self.scope.is_none() => Known {
                base: None,
                source_by: None,
            },
            None => Known {
                base: Some(Held::Unread),
                source_by: None,
            },
        }
    }

    /// Moves the conflict the keys act on.
    pub(super) fn step_conflict(&mut self, forward: bool, cx: &mut Context<Self>) {
        let count = self.conflicts().map_or(0, |conflicts| conflicts.len());
        if count == 0 {
            return;
        }
        let at = self.resolve.selected.min(count - 1);
        self.resolve.selected = if forward {
            (at + 1).min(count - 1)
        } else {
            at.saturating_sub(1)
        };
        self.conflict_list
            .scroll_to_reveal_item(self.resolve.selected);
        cx.notify();
    }

    /// Shows the conflicts list from the selected conflict, or the changes again.
    pub(super) fn show_conflicts_action(&mut self, _: &ShowConflicts, cx: &mut Context<Self>) {
        self.toggle_conflicts(cx);
    }

    /// The sides read for the conflict at `ix`, when they are.
    fn sides_at(&self, ix: usize) -> Option<&Sides> {
        match &self.resolve.state {
            SidesState::Read(read) => read.each.get(ix),
            _ => None,
        }
    }

    /// Why no decision can be written to the branch now, when none can.
    fn branch_write_refusal(&self, cx: &App) -> Option<String> {
        if self.merge_may_land(cx) {
            return Some(MERGE_RUNNING.into());
        }
        if self.branch_deleted {
            return Some(BRANCH_DELETED.into());
        }
        self.change_refusal(cx)
    }

    /// Why no decision can be written to the target now, when none can.
    fn target_write_refusal(&self, cx: &App) -> Option<String> {
        if self.merge_may_land(cx) {
            return Some(MERGE_RUNNING.into());
        }
        if self.resolve.target_out {
            return Some(format!("A write to {} is out: one at a time.", self.target));
        }
        self.session
            .read(cx)
            .refusal(AccessAction::Change, &AccessScope::branch(&self.target))
    }

    /// The write that makes one side hold the other's version of the conflict at `ix`, or why
    /// there is none.
    fn side_write(&self, ix: usize, choice: &Choice, cx: &App) -> Result<ChangeRequest, String> {
        let conflict = self
            .conflicts()
            .and_then(|conflicts| conflicts.get(ix))
            .ok_or("The conflicts were read again.")?;
        let schema = self
            .session
            .read(cx)
            .schema_model()
            .ok_or("The schema has not loaded yet.")?;
        let (branch, query) = match (self.sides_at(ix), choice) {
            (Some(Sides::Entity { target, source, .. }), Choice::Target | Choice::Source) => {
                let id = conflict
                    .entity_id
                    .as_deref()
                    .ok_or("The conflict names no entity.")?;
                if !target.known() || !source.known() {
                    return Err("Not every side could be read.".into());
                }
                let (want, have, branch) = match choice {
                    Choice::Target => (target, source, self.branch.clone()),
                    _ => (source, target, self.target.clone()),
                };
                let is_edge = matches!(conflict.entity_kind, EntityKind::Edge);
                let query = take_side(
                    &schema,
                    is_edge,
                    &conflict.type_name,
                    id,
                    want.image(),
                    have.image(),
                )
                .map_err(|why| why.0)?;
                (branch, query)
            }
            (
                Some(Sides::Card { edge, .. }),
                Choice::Remove {
                    edge: id,
                    on_target,
                },
            ) => {
                let query = builder::delete_edges(&schema, edge, std::slice::from_ref(id))
                    .map_err(|error| error.to_string())?;
                let branch = if *on_target {
                    self.target.clone()
                } else {
                    self.branch.clone()
                };
                (branch, query)
            }
            (None, _) => return Err("Its sides are still being read.".into()),
            _ => return Err("Not offered for this conflict.".into()),
        };
        Ok(ChangeRequest {
            query: query.text.clone(),
            name: Some(query.name.clone()),
            params: query.params_value(),
            branch: Some(branch),
        })
    }

    /// Resolves the selected conflict with the target's version (⌥⌘[), or this branch's
    /// (⌥⌘]), as its buttons do.
    pub(super) fn keep_target(
        &mut self,
        _: &KeepTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ix = self.resolve.selected;
        self.decide(ix, Choice::Target, window, cx);
    }

    pub(super) fn keep_source(
        &mut self,
        _: &KeepSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ix = self.resolve.selected;
        self.decide(ix, Choice::Source, window, cx);
    }

    /// Opens the selected conflict's entity on the branch to edit, with the target's values
    /// beside each field (↵ in the conflicts list).
    pub(super) fn edit_selected_conflict(
        &mut self,
        _: &EditChange,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ix = self.resolve.selected;
        self.edit_conflict(ix, window, cx);
    }

    /// Writes `choice` for the conflict at `ix`: onto the branch at once, as a fix is, or onto
    /// the target once the person confirms it, since it changes what the target holds before
    /// any merge.
    fn decide(&mut self, ix: usize, choice: Choice, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(
            self.resolve.decisions.get(&ix),
            Some(Decision::Writing(_) | Decision::Wrote(..))
        ) {
            return;
        }
        let on_target = match &choice {
            Choice::Source => true,
            Choice::Remove { on_target, .. } => *on_target,
            Choice::Target | Choice::Edit => false,
        };
        let refused = if on_target {
            self.target_write_refusal(cx)
        } else {
            self.branch_write_refusal(cx)
        };
        if refused.is_some() {
            return;
        }
        let request = match self.side_write(ix, &choice, cx) {
            Ok(request) => request,
            Err(why) => {
                self.resolve
                    .decisions
                    .insert(ix, Decision::Unavailable(why));
                cx.notify();
                return;
            }
        };
        let Some(list) = self.resolve.of else { return };
        let what = self.conflict_title(ix);
        if !on_target {
            let key = conflict_key(list, ix);
            self.resolve.pending.insert(key.clone(), ix);
            self.resolve
                .decisions
                .insert(ix, Decision::Writing(choice.clone()));
            let description = match &choice {
                Choice::Remove { .. } => {
                    format!("Remove a relationship of {what} to resolve a conflict")
                }
                _ => format!("Keep {}'s {what}", self.target),
            };
            self.queue_fix(
                Fix {
                    key,
                    request,
                    description,
                    edited: true,
                    after: None,
                },
                cx,
            );
            return;
        }
        let (target, branch) = (self.target.clone(), self.branch.clone());
        let (title, description, action) = match &choice {
            Choice::Remove { .. } => (
                format!("Remove the relationship on {target}?"),
                format!(
                    "This deletes it on {target} now, before any merge, so the merge keeps \
                     {branch}'s. {target}'s history keeps the commit it was added in."
                ),
                format!("Remove on {target}"),
            ),
            _ => (
                format!("Write {branch}'s {what} to {target}?"),
                format!(
                    "This writes {branch}'s values onto {target} now, before any merge, so the \
                     two agree and the merge takes them. {target}'s history keeps what it held."
                ),
                format!("Write to {target}"),
            ),
        };
        let view = cx.entity().downgrade();
        // Run after the update this is asked from: with no workspace to ask in, `confirm` agrees
        // at once.
        studio_workspace::confirm(window, cx, title, description, action, move |_, cx| {
            cx.defer(move |cx| {
                view.update(cx, |this, cx| {
                    this.send_target_write(ix, choice, request, cx)
                })
                .ok();
            });
        });
    }

    /// Sends a decision's write to the target, bound to the target's head the sides were read
    /// at, or the one the last such write left. Never retried.
    fn send_target_write(
        &mut self,
        ix: usize,
        choice: Choice,
        request: ChangeRequest,
        cx: &mut Context<Self>,
    ) {
        let SidesState::Read(read) = &self.resolve.state else {
            return;
        };
        if self.target_write_refusal(cx).is_some() {
            return;
        }
        let bound = read.target_head.clone();
        let list = self.resolve.of;
        let description = match &choice {
            Choice::Remove { .. } => format!(
                "Remove a relationship of {} on {} to resolve a conflict",
                self.conflict_title(ix),
                self.target
            ),
            _ => format!("Keep {}'s {}", self.branch, self.conflict_title(ix)),
        };
        self.resolve.target_out = true;
        self.resolve
            .decisions
            .insert(ix, Decision::Writing(choice.clone()));
        let task = self.session.update(cx, |session, cx| {
            session.write_request(request, Some(bound), description, cx)
        });
        self.resolve._target_task = Some(cx.spawn(async move |this, cx| {
            let outcome = task.await;
            this.update(cx, |this, cx| {
                this.resolve.target_out = false;
                if let (WriteOutcome::Committed(output), SidesState::Read(read)) =
                    (&outcome, &mut this.resolve.state)
                    && let Some(commit) = &output.commit
                {
                    read.target_head = commit.graph_commit_id.clone();
                }
                if this.resolve.of == list {
                    let decision = target_decision(choice, outcome);
                    let landed = matches!(decision, Decision::Wrote(..));
                    this.resolve.decisions.insert(ix, decision);
                    if landed {
                        this.decision_landed(cx);
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// A decision landed: the merge's conflicts are no longer what a merge would meet, so the
    /// dry run is read again once nothing is out, and its answer replaces the list.
    fn decision_landed(&mut self, cx: &mut Context<Self>) {
        if matches!(self.merge, super::MergeState::Conflicts(_)) {
            self.merge = super::MergeState::Idle;
        }
        self.preview_again = true;
        self.writes_settled(cx);
    }

    /// A fix's outcome, for the conflict whose decision it was sent for: a decision written to
    /// the branch, or an edit opened from a conflict.
    pub(super) fn conflict_fix_finished(
        &mut self,
        key: &str,
        fix: &FixState,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.resolve.pending.remove(key) else {
            return;
        };
        let choice = match self.resolve.decisions.get(&ix) {
            Some(Decision::Writing(choice)) => choice.clone(),
            _ => Choice::Edit,
        };
        let decision = match fix {
            FixState::Reverted { commit } | FixState::Edited { commit } => {
                Decision::Wrote(choice, commit.clone())
            }
            FixState::Unchanged => Decision::Unchanged(choice),
            FixState::Moved => Decision::Moved(choice),
            FixState::Refused(why) => Decision::Refused(choice, why.clone()),
            FixState::NotSent(why) => Decision::NotSent(choice, why.clone()),
            FixState::Unknown => Decision::Unknown(choice),
            FixState::Idle | FixState::Running | FixState::Saving => return,
        };
        let landed = matches!(decision, Decision::Wrote(..));
        self.resolve.decisions.insert(ix, decision);
        if landed {
            // The fix's own outcome reads the dry run again once nothing is out
            // (`preview_again`); a merge's conflicts give way to it.
            if matches!(self.merge, super::MergeState::Conflicts(_)) {
                self.merge = super::MergeState::Idle;
            }
        }
        cx.notify();
    }

    /// Opens the conflict's entity on the branch to edit, the target's value beside each field.
    fn edit_conflict(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.branch_write_refusal(cx).is_some() {
            return;
        }
        let Some((change, target)) = self.conflict_edit(ix) else {
            return;
        };
        let key = self
            .data
            .as_ref()
            .and_then(|data| data.folded.changes.get(change))
            .map(|change| change.key());
        let Some(key) = key else { return };
        self.resolve.pending.insert(key, ix);
        self.open_edit_beside(change, Some((self.target.clone(), target)), window, cx);
    }

    /// The review's change of the conflict's entity, which Edit… edits, and the target's image
    /// of it; `None` when it offers no edit: a relationship, an entity the branch removed or
    /// the review does not show, or sides not read.
    fn conflict_edit(&self, ix: usize) -> Option<(usize, Option<SideImage>)> {
        let conflict = self.conflicts()?.get(ix)?;
        if matches!(conflict.entity_kind, EntityKind::Edge) {
            return None;
        }
        let data = self.data.as_ref()?;
        let change = data.change_to(&conflict.type_name, conflict.entity_id.as_deref()?)?;
        data.folded.changes[change].after.as_ref()?;
        let Some(Sides::Entity { target, .. }) = self.sides_at(ix) else {
            return None;
        };
        Some((change, target.image().cloned()))
    }

    /// The conflict's entity in a few words: its type and name, else its id.
    fn conflict_title(&self, ix: usize) -> String {
        let Some(conflict) = self.conflicts().and_then(|conflicts| conflicts.get(ix)) else {
            return String::new();
        };
        let Some(id) = conflict.entity_id.as_deref() else {
            return conflict.type_name.clone();
        };
        let name = match &self.resolve.state {
            SidesState::Read(read) => read
                .names
                .get(&(conflict.type_name.clone(), id.to_owned()))
                .cloned(),
            _ => None,
        };
        format!(
            "{} {}",
            conflict.type_name,
            name.unwrap_or_else(|| id.to_owned())
        )
    }

    // ---- drawing ----

    /// The conflicts list: a merge's, else a dry run's, each a card of its three sides.
    pub(super) fn render_conflicts(&self, cx: &mut Context<Self>) -> AnyElement {
        // The decisions landed, and the merge is being checked again, or merges now.
        if self.conflicts().is_none() {
            let (title, detail) = match &self.preview {
                super::Preview::Reading => (
                    "Checking the merge again…".to_owned(),
                    format!(
                        "Asking whether {} merges into {} now.",
                        self.branch, self.target
                    ),
                ),
                super::Preview::Merges(_) => (
                    "Nothing stops the merge now".to_owned(),
                    format!(
                        "{} merges into {} without a conflict. Review the changes, then merge.",
                        self.branch, self.target
                    ),
                ),
                _ => (
                    "No conflicts to show".to_owned(),
                    "The merge check could not be read. Refresh to read it again.".to_owned(),
                ),
            };
            let icon = match &self.preview {
                super::Preview::Merges(_) => IconName::Check,
                _ => IconName::GitMergeConflict,
            };
            return studio_ui::ds::empty_state(icon, title)
                .child(studio_ui::ds::text::muted(detail))
                .child(
                    studio_ui::ds::button("conflicts-done", "Show changes")
                        .style(ButtonStyle::Outlined)
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_conflicts(cx))),
                )
                .into_any_element();
        }
        let count = self.conflicts().map_or(0, |conflicts| conflicts.len());
        // A merge's own conflicts, or a dry run's read again, replace the list shown.
        if self.conflict_list.item_count() != count {
            self.conflict_list.reset(count);
        }
        let heading = if matches!(self.merge, super::MergeState::Conflicts(_)) {
            format!("Conflicts merging {} into {}", self.branch, self.target)
        } else {
            format!(
                "Conflicts a merge of {} into {} would stop on",
                self.branch, self.target
            )
        };
        let state = match &self.resolve.state {
            SidesState::Reading | SidesState::Unread => Some(
                h_flex()
                    .gap(px(space::INLINE))
                    .child(studio_ui::ds::spinner())
                    .child(studio_ui::ds::text::meta(format!(
                        "Reading what {} and {} hold…",
                        self.target, self.branch
                    )))
                    .into_any_element(),
            ),
            SidesState::Failed(why) => Some(
                studio_ui::ds::text::meta(format!(
                    "What each side holds could not be read: {}",
                    studio_ui::clause(why)
                ))
                .color(Color::Warning)
                .into_any_element(),
            ),
            SidesState::Read(read) => {
                let mut notes = Vec::new();
                if count > SIDES_CAP {
                    notes.push(format!(
                        "Sides read for the first {SIDES_CAP} of {count} conflicts."
                    ));
                }
                if !read.who_whole {
                    notes.push(format!(
                        "Who set {}'s side is read from its newest changes since the fork only.",
                        self.target
                    ));
                }
                (!notes.is_empty())
                    .then(|| studio_ui::ds::text::meta(notes.join(" ")).into_any_element())
            }
        };
        v_flex()
            .flex_1()
            .min_h_0()
            .pt(px(space::XL))
            .gap(px(space::MD))
            .child(
                v_flex()
                    .px(px(space::XL))
                    .gap(px(space::XS))
                    .child(studio_ui::ds::text::muted(heading))
                    .children(state),
            )
            .child(
                gpui::list(
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
        let Some(conflict) = self
            .conflicts()
            .and_then(|conflicts| conflicts.get(ix))
            .cloned()
        else {
            return div().into_any_element();
        };
        // What the branch did to the conflict's entity.
        let here = conflict.entity_id.as_deref().and_then(|id| {
            let data = self.data.as_ref()?;
            data.folded.changes[data.change_to(&conflict.type_name, id)?].net()
        });
        let words = conflict_wording(conflict.kind, here, &self.target);
        let selected = ix == self.resolve.selected;
        let name = conflict
            .entity_id
            .as_deref()
            .and_then(|id| match &self.resolve.state {
                SidesState::Read(read) => read
                    .names
                    .get(&(conflict.type_name.clone(), id.to_owned()))
                    .cloned(),
                _ => None,
            });
        let header = h_flex()
            .w_full()
            .gap(px(space::MD))
            .child(Icon::new(IconName::GitMergeConflict).color(Color::Conflict))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(studio_ui::ds::text::plain(words).color(Color::Conflict)),
            )
            .when(here.is_some(), |row| {
                row.child(
                    studio_ui::ds::row_button(("conflict-show", ix), "Show change")
                        .tip(studio_ui::ds::tooltip(
                            "Show this branch's change on its card",
                        ))
                        .on_click(cx.listener(move |this, _, _, cx| this.open_conflict(ix, cx))),
                )
            });
        let identity = h_flex()
            .gap(px(space::MD))
            .child(type_swatch(&conflict.type_name, cx))
            .child(studio_ui::ds::text::muted(conflict.type_name.clone()))
            .children(name.map(studio_ui::ds::text::body))
            .when_some(conflict.entity_id.clone(), |row, entity| {
                row.child(studio_ui::ds::text::meta(entity).buffer_font(cx))
            });
        let body = match self.sides_at(ix).cloned() {
            Some(Sides::Entity {
                base,
                target,
                source,
                target_by,
                source_by,
            }) => self.render_sides(
                ix,
                &conflict,
                [&base, &target, &source],
                [target_by, source_by],
                cx,
            ),
            Some(Sides::Card { edges, from, edge }) => {
                self.render_card_edges(ix, &edge, &from, &edges, cx)
            }
            Some(Sides::Words) | None => {
                studio_ui::ds::text::meta(conflict.message.clone()).into_any_element()
            }
        };
        let decisions = self.render_decisions(ix, &conflict, cx);
        let card = studio_ui::ds::lifted(v_flex().id(("conflict", ix)), cx)
            .w_full()
            .gap(px(space::MD))
            .px(px(space::LG))
            .py(px(space::MD))
            .rounded(px(radii(cx).md))
            .bg(studio_ui::ds::surface::document(cx))
            .border_1()
            .border_color(if selected {
                studio_ui::ds::surface::focus(cx)
            } else {
                studio_ui::ds::surface::card_border(cx)
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.resolve.selected = ix;
                cx.notify();
            }))
            .child(header)
            .child(identity)
            .child(body)
            .children(decisions);
        div()
            .px(px(space::XL))
            .pb(px(space::MD))
            .child(card)
            .into_any_element()
    }

    /// The three sides of a node or relationship: a column each for the fork, the target now
    /// and this branch, who set the two sides, then each property they disagree on.
    fn render_sides(
        &self,
        ix: usize,
        conflict: &MergeConflictOutput,
        sides: [&Held; 3],
        by: [Option<SetBy>; 2],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let order: Vec<String> = self
            .session
            .read(cx)
            .schema_model()
            .and_then(|schema| {
                let properties = match conflict.entity_kind {
                    EntityKind::Edge => &schema.edge(&conflict.type_name)?.properties,
                    _ => &schema.node(&conflict.type_name)?.properties,
                };
                Some(properties.iter().map(|p| p.name.clone()).collect())
            })
            .unwrap_or_default();
        let (rows, agree) = compared_rows(&order, sides[0], sides[1], sides[2]);
        let now = studio_ui::now_micros();
        let [target_by, source_by] = by;
        let headings = [
            ("At the fork".to_owned(), None),
            (
                format!("{} now", self.target),
                target_by.map(|by| set_by_words(&by, now)),
            ),
            (
                self.branch.clone(),
                source_by.map(|by| set_by_words(&by, now)),
            ),
        ];
        let column = || div().flex_1().min_w_0();
        let label_column = || div().w(px(size::FIELD_LABEL)).flex_none().min_w_0();
        let mut table = v_flex().gap(px(space::XS)).child(
            h_flex()
                .gap(px(space::MD))
                .items_start()
                .child(label_column())
                .children(headings.into_iter().map(|(title, who)| {
                    column().child(
                        v_flex()
                            .child(studio_ui::ds::text::meta(title).truncate())
                            .children(who.map(studio_ui::ds::text::meta)),
                    )
                })),
        );
        let absent_words = |side: usize| -> &'static str {
            match (side, sides[side]) {
                (_, Held::Unread) => "not read",
                (0, _) => "not there yet",
                (_, _) => match here_or_there(sides[0]) {
                    true => "deleted",
                    false => "not there",
                },
            }
        };
        if rows.is_empty() && sides.iter().all(|side| side.known()) {
            table = table.child(studio_ui::ds::text::meta(
                "The sides hold the same values now. Read the merge check again.",
            ));
        }
        for (name, values) in rows {
            let base = values[0].clone();
            table = table.child(
                h_flex()
                    .gap(px(space::MD))
                    .items_start()
                    .child(label_column().child(studio_ui::ds::text::muted(name).truncate()))
                    .children((0..3).map(|side| {
                        let cell = match (sides[side], &values[side]) {
                            (Held::Holds(..), Some(value)) => {
                                let text = value_words(value);
                                let changed = side > 0
                                    && !base.as_ref().is_some_and(|base| same_value(base, value));
                                let label = studio_ui::ds::text::body(text).truncate();
                                if changed {
                                    label.into_any_element()
                                } else {
                                    label.color(Color::Muted).into_any_element()
                                }
                            }
                            (Held::Holds(..), None) => {
                                studio_ui::ds::text::meta("not read").into_any_element()
                            }
                            _ => studio_ui::ds::text::meta(absent_words(side))
                                .italic()
                                .into_any_element(),
                        };
                        column().child(cell)
                    })),
            );
        }
        if agree > 0 {
            table = table.child(h_flex().gap(px(space::MD)).child(label_column()).child(
                studio_ui::ds::text::meta(format!(
                    "{} the sides agree on",
                    studio_ui::plural(agree, "other property", "other properties")
                )),
            ));
        }
        let _ = ix;
        table.into_any_element()
    }

    /// The relationships a `@card` conflict counts: each with its other end, where it is, and
    /// Remove on its side.
    fn render_card_edges(
        &self,
        ix: usize,
        edge: &str,
        from: &str,
        edges: &[CardEdge],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let names = match &self.resolve.state {
            SidesState::Read(read) => Some(&read.names),
            _ => None,
        };
        let to_type = self.session.read(cx).schema_model().and_then(|schema| {
            schema
                .edge(edge)
                .map(|edge| (edge.from.clone(), edge.to.clone()))
        });
        let name_of = |type_name: Option<&String>, id: &str| -> String {
            type_name
                .and_then(|type_name| names?.get(&(type_name.clone(), id.to_owned())))
                .cloned()
                .unwrap_or_else(|| id.to_owned())
        };
        let source_name = name_of(to_type.as_ref().map(|(from, _)| from), from);
        let now = studio_ui::now_micros();
        let target_refused = self.target_write_refusal(cx);
        let branch_refused = self.branch_write_refusal(cx);
        let decided = matches!(
            self.resolve.decisions.get(&ix),
            Some(Decision::Writing(_) | Decision::Wrote(..))
        );
        let mut list = v_flex().gap(px(space::XS)).child(studio_ui::ds::text::meta(format!(
            "{source_name} may hold fewer {edge} relationships than the two sides hold together:"
        )));
        for (n, card) in edges.iter().enumerate() {
            let other = name_of(to_type.as_ref().map(|(_, to)| to), &card.to);
            let place = match (card.on_target, card.on_source) {
                (true, true) => format!("on both {} and {}", self.target, self.branch),
                (true, false) => match &card.by {
                    Some(by) => format!("on {} · {}", self.target, set_by_words(by, now)),
                    None => format!("on {}", self.target),
                },
                (false, true) => format!("on {}", self.branch),
                (false, false) => String::new(),
            };
            let remove = |on_target: bool, label: String, refused: Option<String>| {
                let edge_id = card.id.clone();
                let branch = if on_target {
                    self.target.clone()
                } else {
                    self.branch.clone()
                };
                studio_ui::ds::row_button(
                    ("card-remove", ix * 1024 + n * 2 + on_target as usize),
                    label,
                )
                .style(ButtonStyle::Outlined)
                .disabled(refused.is_some() || decided)
                .tip(studio_ui::ds::tooltip(match refused {
                    Some(why) => why,
                    None => format!("Deletes this relationship on {branch}"),
                }))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.decide(
                        ix,
                        Choice::Remove {
                            edge: edge_id.clone(),
                            on_target,
                        },
                        window,
                        cx,
                    )
                }))
            };
            list = list.child(
                h_flex()
                    .gap(px(space::MD))
                    .child(div().flex_1().min_w_0().child(
                        studio_ui::ds::text::body(format!("{source_name} → {other}")).truncate(),
                    ))
                    .child(studio_ui::ds::text::meta(place).truncate())
                    .when(card.on_target && !card.on_source, |row| {
                        row.child(remove(
                            true,
                            format!("Remove on {}", self.target),
                            target_refused.clone(),
                        ))
                    })
                    .when(card.on_source && !card.on_target, |row| {
                        row.child(remove(false, "Remove here".into(), branch_refused.clone()))
                    }),
            );
        }
        list.into_any_element()
    }

    /// A conflict's decisions and what became of the one taken.
    fn render_decisions(
        &self,
        ix: usize,
        conflict: &MergeConflictOutput,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let decision = self.resolve.decisions.get(&ix).cloned();
        let status = decision
            .as_ref()
            .map(|decision| decision_words(decision, &self.branch, &self.target));
        let Some(Sides::Entity { target, source, .. }) = self.sides_at(ix) else {
            return status.map(|(text, color)| {
                studio_ui::ds::text::meta(text)
                    .color(color)
                    .into_any_element()
            });
        };
        let readable = target.known() && source.known();
        let busy = matches!(decision, Some(Decision::Writing(_) | Decision::Wrote(..)));
        let focus = self.focus_handle.clone();
        let branch_refused = self.branch_write_refusal(cx);
        let target_refused = self.target_write_refusal(cx);
        let unreadable = (!readable).then(|| "Not every side could be read.".to_owned());
        let keep_target = {
            let refused = branch_refused.clone().or(unreadable.clone());
            let focus = focus.clone();
            let (target, branch) = (self.target.clone(), self.branch.clone());
            studio_ui::ds::row_button(("keep-target", ix), format!("Keep {}'s", self.target))
                .style(ButtonStyle::Outlined)
                .disabled(busy || refused.is_some())
                .tip_fn(move |_, cx| match &refused {
                    Some(why) => studio_ui::ds::tooltip_view(why.clone(), cx),
                    None => studio_ui::ds::tooltip_for_action_in(
                        format!("Write {target}'s version onto {branch}"),
                        &KeepTarget,
                        &focus,
                    )
                    .meta(format!(
                        "Writes to {branch}, bound to the head the review's fixes bind to"
                    ))
                    .into_view(cx),
                })
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.decide(ix, Choice::Target, window, cx)
                }))
        };
        let keep_source = {
            let refused = target_refused.or(unreadable);
            let focus = focus.clone();
            let (target, branch) = (self.target.clone(), self.branch.clone());
            studio_ui::ds::row_button(("keep-source", ix), format!("Keep {}'s…", self.branch))
                .style(ButtonStyle::Outlined)
                .disabled(busy || refused.is_some())
                .tip_fn(move |_, cx| match &refused {
                    Some(why) => studio_ui::ds::tooltip_view(why.clone(), cx),
                    None => studio_ui::ds::tooltip_for_action_in(
                        format!("Write {branch}'s version onto {target}, once you confirm"),
                        &KeepSource,
                        &focus,
                    )
                    .meta(format!(
                        "Writes to {target} before the merge; a merge that takes a side (E61) would not"
                    ))
                    .into_view(cx),
                })
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.decide(ix, Choice::Source, window, cx)
                }))
        };
        let editable = self.conflict_edit(ix).is_some();
        let edit = editable.then(|| {
            let refused = branch_refused.clone();
            let branch = self.branch.clone();
            let target = self.target.clone();
            studio_ui::ds::row_button(("conflict-edit", ix), "Edit…")
                .style(ButtonStyle::Outlined)
                .start_icon(Icon::new(IconName::Pencil))
                .disabled(busy || refused.is_some())
                .tip_fn(move |_, cx| match &refused {
                    Some(why) => studio_ui::ds::tooltip_view(why.clone(), cx),
                    None => studio_ui::ds::tooltip_for_action_in(
                        format!("Edit what {branch} holds, with {target}'s values beside it"),
                        &EditChange,
                        &focus,
                    )
                    .into_view(cx),
                })
                .on_click(
                    cx.listener(move |this, _, window, cx| this.edit_conflict(ix, window, cx)),
                )
        });
        let _ = conflict;
        Some(
            h_flex()
                .gap(px(space::SM))
                .flex_wrap()
                .child(keep_target)
                .child(keep_source)
                .children(edit)
                .child(div().flex_1())
                .children(status.map(|(text, color)| {
                    studio_ui::ds::text::meta(text)
                        .color(color)
                        .into_any_element()
                }))
                .into_any_element(),
        )
    }
}

/// Whether the fork held the entity: a side without it then deleted it.
fn here_or_there(base: &Held) -> bool {
    matches!(base, Held::Holds(..))
}

/// A value as a card shows it on one line.
fn value_words(value: &Value) -> String {
    match value {
        Value::Null => "—".to_owned(),
        other => studio_ui::value_text(other).replace('\n', " "),
    }
}

/// Who set a side, in a few words: "act-writer on tentative/scout-helix, merged by act-admin ·
/// 2 h ago".
pub(super) fn set_by_words(by: &SetBy, now: i64) -> String {
    let mut words = by.actor.clone().unwrap_or_else(|| "someone".into());
    if let Some(from) = &by.from {
        words.push_str(&format!(" on {from}"));
    }
    if let Some(merged_by) = &by.merged_by
        && Some(merged_by) != by.actor.as_ref()
    {
        words.push_str(&format!(", merged by {merged_by}"));
    }
    if by.at > 0 {
        words.push_str(" · ");
        words.push_str(&studio_ui::relative_time(by.at, now));
    }
    words
}

/// What a decision's state says, and in what colour.
fn decision_words(decision: &Decision, branch: &str, target: &str) -> (String, Color) {
    let taken = |choice: &Choice| match choice {
        Choice::Target => format!("{target}'s version"),
        Choice::Source => format!("{branch}'s version"),
        Choice::Edit => "the edit".to_owned(),
        Choice::Remove {
            on_target: true, ..
        } => format!("the removal on {target}"),
        Choice::Remove { .. } => format!("the removal on {branch}"),
    };
    let written_to = |choice: &Choice| match choice {
        Choice::Source
        | Choice::Remove {
            on_target: true, ..
        } => target,
        _ => branch,
    };
    match decision {
        Decision::Writing(choice) => (
            format!("Writing {} to {}…", taken(choice), written_to(choice)),
            Color::Muted,
        ),
        Decision::Wrote(choice, commit) => (
            format!(
                "Wrote {} to {} in {}. Checking the merge again.",
                taken(choice),
                written_to(choice),
                short_commit_id(commit)
            ),
            Color::Success,
        ),
        Decision::Unchanged(_) => (
            "Not written: a value changed since the sides were read. Refresh.".to_owned(),
            Color::Warning,
        ),
        Decision::Moved(choice) => (
            format!(
                "Not written: {} moved. Refresh, then try again.",
                written_to(choice)
            ),
            Color::Warning,
        ),
        Decision::Refused(_, why) => (format!("Refused: {why}"), Color::Error),
        Decision::NotSent(_, why) => (
            format!("Not sent: {}. Nothing was written.", studio_ui::clause(why)),
            Color::Error,
        ),
        Decision::Unknown(choice) => (
            format!(
                "Outcome unknown. Check {}'s history before trying again.",
                written_to(choice)
            ),
            Color::Warning,
        ),
        Decision::Unavailable(why) => (why.clone(), Color::Muted),
    }
}

/// A write to the target's outcome, as a decision.
fn target_decision(choice: Choice, outcome: WriteOutcome<ChangeOutput>) -> Decision {
    match outcome {
        WriteOutcome::Committed(output) => match output.commit {
            Some(commit) if output.affected_nodes + output.affected_edges > 0 => {
                Decision::Wrote(choice, commit.graph_commit_id)
            }
            _ => Decision::Unchanged(choice),
        },
        WriteOutcome::Refused(error) if error.status == 412 => Decision::Moved(choice),
        WriteOutcome::Refused(error) => Decision::Refused(choice, error.reason().to_owned()),
        WriteOutcome::NotSent { reason } => Decision::NotSent(choice, reason),
        WriteOutcome::Unknown { .. } => Decision::Unknown(choice),
    }
}

/// The rows of a `nodes_fields_by_id` or `edges_of` read, by id, as side images: each
/// property's column turned back to its name.
fn images_of(rows: &Value, edge: bool, properties: &[String]) -> HashMap<String, ReadImage> {
    let mut images = HashMap::new();
    for row in rows.as_array().into_iter().flatten() {
        let Some(row) = row.as_object() else { continue };
        let id_column = if edge { "edge_id" } else { "id" };
        let Some(id) = row.get(id_column).and_then(Value::as_str) else {
            continue;
        };
        let prefix = if edge { "e_" } else { "v_" };
        let mut image: Map<String, Value> = row
            .iter()
            .filter_map(|(column, value)| {
                Some((column.strip_prefix(prefix)?.to_owned(), value.clone()))
            })
            .collect();
        for name in properties {
            image.entry(name.clone()).or_insert(Value::Null);
        }
        let ends = edge.then(|| {
            let end = |column: &str| row.get(column).and_then(Value::as_str).map(str::to_owned);
            end("from_id").zip(end("to_id"))
        });
        images.insert(id.to_owned(), (image, ends.flatten()));
    }
    images
}

/// Reads the conflicts' sides (see [`ReviewView::read_sides`]).
async fn read_sides(
    session: &Entity<GraphSession>,
    plan: Plan,
    cx: &mut AsyncApp,
) -> Result<SidesRead, String> {
    // The merge base, both heads, as the server names them now: the target's head is the one
    // a write to it binds to.
    let base = session
        .update(cx, |session, cx| {
            session.merge_base(&plan.branch, &plan.target, cx)
        })
        .await
        .map_err(|error| error.to_string())?;
    let target_head = base.target_commit_id.clone();
    // The branch's side is read at the head the review's fixes bind to, which its decisions
    // bind to too; the server's when the review holds none.
    let source_head = plan
        .source_head
        .clone()
        .unwrap_or_else(|| base.source_commit_id.clone());
    let schema = plan.schema.clone();
    // Each type's conflicting entities, read in one query at each head.
    let mut by_type: HashMap<(bool, String), Vec<String>> = HashMap::new();
    for conflict in &plan.conflicts {
        if matches!(conflict.kind, MergeConflictKind::CardinalityViolation) {
            continue;
        }
        if let Some(id) = &conflict.entity_id {
            let edge = matches!(conflict.entity_kind, EntityKind::Edge);
            let ids = by_type
                .entry((edge, conflict.type_name.clone()))
                .or_default();
            if !ids.contains(id) {
                ids.push(id.clone());
            }
        }
    }
    // (edge, type, id) → what each head holds; a type whose read failed is left out, and its
    // entities read as not read.
    let mut at_target: HeldAt = HashMap::new();
    let mut at_source = at_target.clone();
    for ((edge, type_name), ids) in &by_type {
        // Every property a projection reads: a row leaves out a column whose value is null,
        // which is a null, not a property unread.
        let properties: Vec<String> = if *edge {
            schema.edge(type_name).map(|edge| &edge.properties)
        } else {
            schema.node(type_name).map(|node| &node.properties)
        }
        .map(|properties| {
            properties
                .iter()
                .filter(|p| p.projectable())
                .map(|p| p.name.clone())
                .collect()
        })
        .unwrap_or_default();
        let query = if *edge {
            builder::edges_of(&schema, type_name, EdgesOf::Ids(ids), ids.len() as u32)
        } else {
            builder::nodes_fields_by_id(&schema, type_name, ids, &properties)
        };
        let Ok(query) = query else { continue };
        for (head, held) in [
            (&target_head, &mut at_target),
            (&source_head, &mut at_source),
        ] {
            let read = session
                .update(cx, |session, cx| {
                    session.read(&query, Some(head.clone()), cx)
                })
                .await;
            if let Ok(answer) = read {
                let mut images = images_of(&answer.rows, *edge, &properties);
                for id in ids {
                    held.insert((*edge, type_name.clone(), id.clone()), images.remove(id));
                }
            }
        }
    }
    // The relationships each `@card` conflict counts, on each side.
    let mut cards: HashMap<usize, (String, String, Vec<CardEdge>)> = HashMap::new();
    for (ix, conflict) in plan.conflicts.iter().enumerate() {
        if !matches!(conflict.kind, MergeConflictKind::CardinalityViolation) {
            continue;
        }
        let Some(from) = card_source(&conflict.message) else {
            continue;
        };
        let Ok(query) = builder::edges_of(
            &schema,
            &conflict.type_name,
            EdgesOf::From(from),
            CARD_EDGES,
        ) else {
            continue;
        };
        let mut edges: Vec<CardEdge> = Vec::new();
        for (head, on_target) in [(&target_head, true), (&source_head, false)] {
            let read = session
                .update(cx, |session, cx| {
                    session.read(&query, Some(head.clone()), cx)
                })
                .await;
            let Ok(answer) = read else { continue };
            for (id, (_, ends)) in images_of(&answer.rows, true, &[]) {
                let to = ends.map(|(_, to)| to).unwrap_or_default();
                match edges.iter_mut().find(|edge| edge.id == id) {
                    Some(edge) if on_target => edge.on_target = true,
                    Some(edge) => edge.on_source = true,
                    None => edges.push(CardEdge {
                        id,
                        to,
                        on_target,
                        on_source: !on_target,
                        by: None,
                    }),
                }
            }
        }
        edges.sort_by(|a, b| a.id.cmp(&b.id));
        cards.insert(ix, (conflict.type_name.clone(), from.to_owned(), edges));
    }
    // Who set the target's side: its feed since the fork, of the conflicts' types, the newest
    // change to each entity kept.
    let mut types: Vec<String> = plan
        .conflicts
        .iter()
        .map(|conflict| conflict.type_name.clone())
        .collect();
    types.sort();
    types.dedup();
    let wanted: HashSet<(String, String)> =
        plan.conflicts
            .iter()
            .filter_map(|conflict| Some((conflict.type_name.clone(), conflict.entity_id.clone()?)))
            .chain(cards.values().flat_map(|(edge, _, edges)| {
                edges.iter().map(|card| (edge.clone(), card.id.clone()))
            }))
            .collect();
    let fork = plan
        .fork
        .clone()
        .unwrap_or_else(|| base.merge_base.graph_commit_id.clone());
    let (mut who, who_whole) = read_who(
        session,
        &plan.target,
        &fork,
        &target_head,
        types,
        &wanted,
        cx,
    )
    .await;
    let mut merged: HashMap<String, (Option<String>, Option<String>)> = HashMap::new();
    for set in who.values() {
        if let Some(parent) = &set.from
            && !merged.contains_key(parent)
            && merged.len() < MERGED_FROM_READS
        {
            let commit = session
                .update(cx, |session, cx| session.commit(parent, cx))
                .await;
            merged.insert(
                parent.clone(),
                commit
                    .ok()
                    .map(|commit| (commit.graph_branch, commit.actor_id))
                    .unwrap_or_default(),
            );
        }
    }
    // A change a merge brought names the merged branch and its head's author.
    for set in who.values_mut() {
        if let Some(parent) = set.from.take() {
            let (branch, actor) = merged.get(&parent).cloned().unwrap_or_default();
            set.merged_by = set.actor.take();
            set.actor = actor.or_else(|| set.merged_by.clone());
            set.from = branch;
        }
    }
    let each: Vec<Sides> = plan
        .conflicts
        .iter()
        .enumerate()
        .map(|(ix, conflict)| {
            if let Some((edge, from, mut edges)) = cards.remove(&ix) {
                for card in &mut edges {
                    if card.on_target && !card.on_source {
                        card.by = who.get(&(edge.clone(), card.id.clone())).cloned();
                    }
                }
                return Sides::Card { edge, from, edges };
            }
            let Some(id) = conflict.entity_id.clone() else {
                return Sides::Words;
            };
            let edge = matches!(conflict.entity_kind, EntityKind::Edge);
            let key = (edge, conflict.type_name.clone(), id.clone());
            let held = |at: &HeldAt| match at.get(&key) {
                Some(Some((image, ends))) => Held::Holds(image.clone(), ends.clone()),
                Some(None) => Held::Absent,
                None => Held::Unread,
            };
            let (target, source) = (held(&at_target), held(&at_source));
            let known = &plan.known[ix];
            let base = match &known.base {
                Some(base) => base.clone(),
                // Unchanged on the branch since the fork: the fork's is the branch's.
                None => source.clone(),
            };
            Sides::Entity {
                base,
                target,
                source,
                target_by: who.get(&(conflict.type_name.clone(), id)).cloned(),
                source_by: known.source_by.clone().map(|by| SetBy { from: None, ..by }),
            }
        })
        .collect();
    // The names of the conflicting nodes and the relationships' ends, at the target's head.
    let mut ends: Vec<(String, String)> = Vec::new();
    for conflict in &plan.conflicts {
        if let (EntityKind::Node, Some(id)) = (&conflict.entity_kind, &conflict.entity_id) {
            ends.push((conflict.type_name.clone(), id.clone()));
        }
    }
    for sides in &each {
        if let Sides::Card { edge, from, edges } = sides
            && let Some(edge) = schema.edge(edge)
        {
            ends.push((edge.from.clone(), from.clone()));
            ends.extend(edges.iter().map(|card| (edge.to.clone(), card.to.clone())));
        }
    }
    ends.sort();
    ends.dedup();
    let (mut names, _) = session
        .update(cx, |session, cx| {
            session.node_names(ends.clone(), Some(target_head.clone()), cx)
        })
        .await;
    // Ends only the branch holds are named there.
    let missing: Vec<_> = ends
        .into_iter()
        .filter(|end| !names.contains_key(end))
        .collect();
    if !missing.is_empty() {
        let (more, _) = session
            .update(cx, |session, cx| {
                session.node_names(missing, Some(source_head.clone()), cx)
            })
            .await;
        names.extend(more);
    }
    Ok(SidesRead {
        target_head,
        each,
        names,
        who_whole,
    })
}

/// Who last changed each of `wanted` (`(type, id)`) on `target` since `fork`: from its feed of
/// `types`, up to [`WHO_PAGES`] pages; a change a merge brought is held with the merged head in
/// `from`, read by id after. With it, whether the feed was read through to `head`.
async fn read_who(
    session: &Entity<GraphSession>,
    target: &str,
    fork: &str,
    head: &str,
    types: Vec<String>,
    wanted: &HashSet<(String, String)>,
    cx: &mut AsyncApp,
) -> (HashMap<(String, String), SetBy>, bool) {
    let mut who = HashMap::new();
    let start = format!("after:{fork}");
    let mut token: Option<String> = None;
    for _ in 0..WHO_PAGES {
        let page = session
            .update(cx, |session, cx| {
                session.typed_changes(
                    target,
                    &start,
                    token.clone(),
                    types.clone(),
                    super::FEED_PAGE as u32,
                    cx,
                )
            })
            .await;
        // A fork off the target's first-parent chain is refused as a start: who set it is not
        // said, and the values still are.
        let Ok(page) = page else {
            return (who, false);
        };
        let mut reached = false;
        for block in &page.blocks {
            reached |= block.cause.graph_commit_id == head;
            for change in &block.changes {
                let key = (change.entity_type.name.clone(), change.id.clone());
                if wanted.contains(&key) {
                    who.insert(
                        key,
                        SetBy {
                            actor: block.cause.actor_id.clone(),
                            from: block.cause.merged_parent_commit_id.clone(),
                            merged_by: None,
                            at: block.cause.authored_at,
                        },
                    );
                }
            }
        }
        match page.next_page_token {
            Some(next) => token = Some(next),
            None => return (who, reached || page.caught_up != Some(false)),
        }
    }
    (who, false)
}

/// The conflicts list as a view of its own, drawn cached and again when the review notifies
/// (it observes it), as [`super::ReviewRows`] is (`docs/drawing.md`): a key typed in the
/// review's search draws the review around the field and leaves the cards as they were.
pub(crate) struct ConflictRows {
    review: gpui::WeakEntity<ReviewView>,
    _changed: gpui::Subscription,
    /// How often the list was drawn, for the test that typing leaves it.
    #[cfg(test)]
    pub(crate) renders: usize,
}

impl ConflictRows {
    /// Made with the review, not on its first draw.
    pub(super) fn new(cx: &mut Context<ReviewView>) -> Entity<Self> {
        use gpui::AppContext as _;
        let review = cx.entity();
        cx.new(|cx| ConflictRows {
            review: review.downgrade(),
            _changed: cx.observe(&review, |_, _, cx| cx.notify()),
            #[cfg(test)]
            renders: 0,
        })
    }
}

impl gpui::Render for ConflictRows {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(test)]
        {
            self.renders += 1;
        }
        let list = self
            .review
            .update(cx, |review, cx| review.render_conflicts(cx))
            .unwrap_or_else(|_| gpui::Empty.into_any_element());
        v_flex().size_full().child(list)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_card_conflict_names_its_source_from_the_servers_words() {
        assert_eq!(
            card_source(
                "@card violation on edge SupersededBy: source 'checkout-launch-nov-13' has 2 edges (max 1)"
            ),
            Some("checkout-launch-nov-13")
        );
        assert_eq!(
            card_source("source 'it's' has 3 edges (max 1)"),
            Some("it's")
        );
        assert_eq!(card_source("divergent update for id 'x'"), None);
    }

    /// A card compares the properties the sides disagree on, in the schema's order, and counts
    /// those they agree on; a side without the entity is shown beside every value the others
    /// hold.
    #[test]
    fn a_card_compares_what_the_sides_disagree_on() {
        let image = |v: Value| Held::Holds(v.as_object().unwrap().clone(), None);
        let order = ["slug", "name", "stage", "amount"].map(str::to_owned);
        let base =
            image(json!({"slug": "d", "name": "Helix", "stage": "in-diligence", "amount": 5}));
        let target =
            image(json!({"slug": "d", "name": "Helix", "stage": "ic-ready", "amount": 5.0}));
        let source = image(json!({"slug": "d", "name": "Helix", "stage": "dead", "amount": 5}));
        let (rows, agree) = compared_rows(&order, &base, &target, &source);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "stage");
        assert_eq!(
            rows[0].1,
            [
                Some(json!("in-diligence")),
                Some(json!("ic-ready")),
                Some(json!("dead"))
            ]
        );
        assert_eq!(agree, 3, "5 and 5.0 are one value");

        // Deleted on the target: every value the others hold is shown, nulls left out.
        let base = image(json!({"slug": "d", "stage": "a", "note": null}));
        let (rows, agree) = compared_rows(&order, &base, &Held::Absent, &base);
        let names: Vec<&str> = rows.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["slug", "stage"]);
        assert_eq!(agree, 0);
    }

    #[test]
    fn who_set_a_side_reads_as_a_phrase() {
        let by = SetBy {
            actor: Some("act-writer".into()),
            from: Some("tentative/scout-helix".into()),
            merged_by: Some("act-admin".into()),
            at: 0,
        };
        assert_eq!(
            set_by_words(&by, 0),
            "act-writer on tentative/scout-helix, merged by act-admin"
        );
        let by = SetBy {
            actor: Some("act-writer".into()),
            at: 1,
            ..SetBy::default()
        };
        assert_eq!(set_by_words(&by, 1), "act-writer · just now");
    }

    #[test]
    fn a_reads_rows_become_images_by_id() {
        let rows = json!([{ "id": "d1", "v_stage": "dead", "v_name": null }]);
        let images = images_of(
            &rows,
            false,
            &["stage".to_owned(), "name".to_owned(), "note".to_owned()],
        );
        assert_eq!(
            images["d1"].0,
            // A column a row leaves out is a null.
            json!({"stage": "dead", "name": null, "note": null})
                .as_object()
                .unwrap()
                .clone()
        );
        let rows = json!([{ "edge_id": "e1", "from_id": "a", "to_id": "b", "e_reason": "x" }]);
        let images = images_of(&rows, true, &[]);
        assert_eq!(images["e1"].1, Some(("a".to_owned(), "b".to_owned())));
        assert_eq!(images["e1"].0["reason"], json!("x"));
    }

    // ---- the review with conflicts, against a fake server ----

    use std::sync::Mutex;

    use gpui::{Focusable as _, TestAppContext, VisualTestContext};

    use crate::view::tests::fake_server;

    const SCHEMA: &str = "node Deal {\n  slug: String @key\n  name: String?\n  stage: String?\n}\n\
        node Decision {\n  slug: String @key\n  title: String?\n}\n\
        edge SupersededBy: Decision -> Decision @card(0..1)\n";

    /// A graph where `feature` forked from main at m1 and moved deal d1 to `dead` (f1), while
    /// main took `ic-ready` from `tentative/scout` by a merge (m2), and both added a successor
    /// to decision c13. Every request is recorded as `path?query body`.
    fn conflicted_server(
        conflicts: Value,
        cx: &mut TestAppContext,
    ) -> (Entity<GraphSession>, Arc<Mutex<Vec<String>>>) {
        let sent: Arc<Mutex<Vec<String>>> = Arc::default();
        let seen = sent.clone();
        let dry_runs = std::sync::atomic::AtomicUsize::new(0);
        let commit = |id: &str| json!({ "graph_commit_id": id, "graph_manifest_version": 1, "kind": "published", "created_at": 0 });
        let deal = |stage: &str| json!({ "slug": "d1", "name": "Helix", "stage": stage });
        let (session, _) = fake_server(
            move |path: &str, query: &str, body: &str| {
                seen.lock().unwrap().push(format!("{path}?{query} {body}"));
                let read = |rows: Value| {
                    (
                        200,
                        json!({ "query_name": "q", "target": {}, "row_count": rows.as_array().map_or(0, Vec::len), "rows": rows }),
                    )
                };
                if path.ends_with("/schema") {
                    (200, json!({ "schema_source": SCHEMA }))
                } else if path.ends_with("/branches/merge-base") {
                    (
                        200,
                        json!({ "merge_base": commit("m1"), "source": "feature", "source_commit_id": "f1", "target": "main", "target_commit_id": "m2" }),
                    )
                } else if path.ends_with("/branches/merge") {
                    // The first dry run meets the conflicts; once one is resolved, none.
                    if dry_runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                        (
                            409,
                            json!({ "error": "conflicts", "code": "conflict", "disposition": { "effects": "none", "action": "refresh" }, "merge_conflicts": conflicts }),
                        )
                    } else {
                        (
                            200,
                            json!({ "source": "feature", "target": "main", "outcome": "merged", "commit": null, "dry_run": true }),
                        )
                    }
                } else if path.ends_with("/changes") && query.contains("branch=feature") {
                    let before = json!({ "properties": deal("in-diligence") });
                    let after = json!({ "properties": deal("dead") });
                    (
                        200,
                        json!({ "blocks": [{ "cause": { "graph_commit_id": "f1", "parent_commit_id": "m1", "authored_branch": "feature", "authored_at": 0, "actor_id": "act-writer" },
                        "changes": [{ "kind": "node", "type": { "id": "Deal", "name": "Deal" }, "id": "d1", "op": "update", "before": before, "after": after }] }],
                        "cursor": "c", "caught_up": true }),
                    )
                } else if path.ends_with("/changes") && query.contains("branch=main") {
                    // Main's side came by the merge of tentative/scout.
                    let after = json!({ "properties": deal("ic-ready") });
                    (
                        200,
                        json!({ "blocks": [{ "cause": { "graph_commit_id": "m2", "parent_commit_id": "m1", "merged_parent_commit_id": "s1", "authored_branch": "main", "authored_at": 0, "actor_id": "act-admin" },
                        "changes": [{ "kind": "node", "type": { "id": "Deal", "name": "Deal" }, "id": "d1", "op": "update", "before": null, "after": after },
                                    { "kind": "edge", "type": { "id": "SupersededBy", "name": "SupersededBy" }, "id": "e20", "op": "insert", "before": null, "after": null }] }],
                        "cursor": "c", "caught_up": true }),
                    )
                } else if path.ends_with("/commits/s1") {
                    let mut scout = commit("s1");
                    scout["graph_branch"] = json!("tentative/scout");
                    scout["actor_id"] = json!("act-scout");
                    (200, scout)
                } else if path.ends_with("/query") {
                    let at_main = body.contains("\"snapshot\":\"m2\"");
                    if body.contains("studio_nodes_fields") {
                        let stage = if at_main { "ic-ready" } else { "dead" };
                        read(
                            json!([{ "id": "d1", "v_slug": "d1", "v_name": "Helix", "v_stage": stage }]),
                        )
                    } else if body.contains("studio_edges_of") {
                        let (id, to) = if at_main {
                            ("e20", "nov-20")
                        } else {
                            ("e27", "nov-27")
                        };
                        read(json!([{ "edge_id": id, "from_id": "c13", "to_id": to }]))
                    } else {
                        read(json!([]))
                    }
                } else if path.contains("/mutate") {
                    (
                        200,
                        json!({ "branch": "x", "query_name": "q", "affected_nodes": 1, "affected_edges": 1,
                        "commit": { "graph_commit_id": "w1", "graph_manifest_version": 9, "kind": "published", "created_at": 0 } }),
                    )
                } else if path.ends_with("/commits") {
                    (200, json!({ "commits": [] }))
                } else {
                    (200, json!({ "branches": ["main", "feature"] }))
                }
            },
            cx,
        );
        (session, sent)
    }

    fn divergent_deal() -> Value {
        json!([{ "entity_kind": "node", "type_name": "Deal", "entity_id": "d1", "kind": "divergent_update", "message": "divergent update for id 'd1'" }])
    }

    /// A review on `conflicted_server`, its conflicts shown and their sides read.
    fn review_with_conflicts(
        conflicts: Value,
        cx: &mut TestAppContext,
    ) -> (
        Entity<ReviewView>,
        Arc<Mutex<Vec<String>>>,
        &mut VisualTestContext,
    ) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
            cx.set_reduce_motion(true);
        });
        let (session, sent) = conflicted_server(conflicts, cx);
        let (view, cx) = cx.add_window_view(|window, cx| {
            ReviewView::new(session, "feature".into(), "main".into(), window, cx)
        });
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert!(view.conflicts().is_some(), "the dry run met the conflicts");
            window.focus(&view.focus_handle, cx);
            view.toggle_conflicts(cx);
        });
        cx.run_until_parked();
        (view, sent, cx)
    }

    /// The conflicts list reads each conflict's three sides: the fork's from the review's
    /// changes, main's and this branch's at their heads, and who set main's, through the merge
    /// that brought it. "Keep main's" writes main's values onto the branch, bound to the head
    /// the fixes bind to, and the merge is checked again.
    #[gpui::test]
    async fn a_conflict_shows_three_sides_and_keeps_mains(cx: &mut TestAppContext) {
        let (view, sent, cx) = review_with_conflicts(divergent_deal(), cx);
        view.read_with(cx, |view, _| {
            let Some(Sides::Entity {
                base,
                target,
                source,
                target_by,
                ..
            }) = view.sides_at(0)
            else {
                panic!("an entity's sides");
            };
            let stage = |held: &Held| held.image().and_then(|image| image.get("stage").cloned());
            assert_eq!(stage(base), Some(json!("in-diligence")));
            assert_eq!(stage(target), Some(json!("ic-ready")));
            assert_eq!(stage(source), Some(json!("dead")));
            let by = target_by.clone().expect("who set main's");
            assert_eq!(by.actor.as_deref(), Some("act-scout"));
            assert_eq!(by.from.as_deref(), Some("tentative/scout"));
            assert_eq!(by.merged_by.as_deref(), Some("act-admin"));
        });
        let asked = sent.lock().unwrap().clone();
        assert!(
            asked.iter().any(|r| r.contains("branch=main")
                && r.contains("type=Deal")
                && r.contains("start=after%3Am1")),
            "main's feed since the fork, of the conflicts' types: {asked:?}"
        );
        sent.lock().unwrap().clear();
        cx.dispatch_action(KeepTarget);
        cx.run_until_parked();
        let asked = sent.lock().unwrap().clone();
        let write = asked
            .iter()
            .find(|r| r.contains("/mutate"))
            .expect("a write");
        assert!(write.contains("/mutate/if-graph-commit"), "bound: {write}");
        assert!(write.contains("\"branch\":\"feature\""), "{write}");
        assert!(
            write.contains("update Deal set { stage: $new0_0 }"),
            "{write}"
        );
        assert!(write.contains("\"new0_0\":\"ic-ready\""), "{write}");
        assert!(write.contains("\"old0_0\":\"dead\""), "{write}");
        assert!(
            asked.iter().any(|r| r.contains("/branches/merge? ")),
            "the merge is checked again: {asked:?}"
        );
        view.read_with(cx, |view, _| {
            assert!(view.conflicts().is_none(), "nothing stops the merge now");
            assert!(view.show_conflicts, "the list stays, saying so");
        });
    }

    /// "Keep this branch's" writes the branch's values onto main, bound to main's head as the
    /// sides were read, once confirmed (a view drawn without a workspace confirms at once).
    #[gpui::test]
    async fn keeping_this_branchs_writes_to_the_target_bound_to_its_head(cx: &mut TestAppContext) {
        let (_view, sent, cx) = review_with_conflicts(divergent_deal(), cx);
        sent.lock().unwrap().clear();
        cx.dispatch_action(KeepSource);
        cx.run_until_parked();
        let asked = sent.lock().unwrap().clone();
        let write = asked
            .iter()
            .find(|r| r.contains("/mutate"))
            .expect("a write");
        assert!(write.contains("\"branch\":\"main\""), "{write}");
        assert!(write.contains("\"new0_0\":\"dead\""), "{write}");
        assert!(write.contains("\"old0_0\":\"ic-ready\""), "{write}");
    }

    /// Too many relationships from one node: the card lists each side's, with its other end, and
    /// removes the branch's on the branch.
    #[gpui::test]
    async fn a_card_conflict_lists_each_sides_relationships(cx: &mut TestAppContext) {
        let conflicts = json!([{ "entity_kind": "edge", "type_name": "SupersededBy", "entity_id": null, "kind": "cardinality_violation",
            "message": "@card violation on edge SupersededBy: source 'c13' has 2 edges (max 1)" }]);
        let (view, sent, cx) = review_with_conflicts(conflicts, cx);
        view.read_with(cx, |view, _| {
            let Some(Sides::Card { edge, from, edges }) = view.sides_at(0) else {
                panic!("a card conflict's sides");
            };
            assert_eq!((edge.as_str(), from.as_str()), ("SupersededBy", "c13"));
            let places: Vec<(&str, &str, bool, bool)> = edges
                .iter()
                .map(|e| (e.id.as_str(), e.to.as_str(), e.on_target, e.on_source))
                .collect();
            assert_eq!(
                places,
                [
                    ("e20", "nov-20", true, false),
                    ("e27", "nov-27", false, true)
                ]
            );
            assert_eq!(
                edges[0].by.as_ref().and_then(|by| by.from.as_deref()),
                Some("tentative/scout")
            );
        });
        sent.lock().unwrap().clear();
        view.update_in(cx, |view, window, cx| {
            view.decide(
                0,
                Choice::Remove {
                    edge: "e27".into(),
                    on_target: false,
                },
                window,
                cx,
            )
        });
        cx.run_until_parked();
        let asked = sent.lock().unwrap().clone();
        let write = asked
            .iter()
            .find(|r| r.contains("/mutate"))
            .expect("a write");
        assert!(write.contains("\"branch\":\"feature\""), "{write}");
        assert!(
            write.contains("delete SupersededBy where @id in $ids"),
            "{write}"
        );
        assert!(write.contains("e27"), "{write}");
    }

    /// The conflicts list is a cached view of its own: keys typed in the review's search draw
    /// it no times; J and K draw it once a key.
    #[gpui::test]
    async fn typing_in_the_search_leaves_the_conflicts_as_they_were(cx: &mut TestAppContext) {
        let (view, _sent, cx) = review_with_conflicts(divergent_deal(), cx);
        let renders = |cx: &mut VisualTestContext| {
            view.read_with(cx, |view, cx| view.conflicts_view.read(cx).renders)
        };
        assert!(renders(cx) > 0, "the list is drawn");
        view.update_in(cx, |view, window, cx| {
            window.focus(&view.search.focus_handle(cx), cx)
        });
        cx.simulate_input("d");
        cx.run_until_parked();
        let before = renders(cx);
        cx.simulate_input("ea");
        cx.run_until_parked();
        assert_eq!(renders(cx), before, "the search field drew the conflicts");
        // A move between conflicts draws it once.
        view.update_in(cx, |view, window, cx| window.focus(&view.focus_handle, cx));
        cx.run_until_parked();
        let before = renders(cx);
        view.update(cx, |view, cx| view.step_conflict(true, cx));
        cx.run_until_parked();
        assert_eq!(renders(cx), before + 1);
    }
}
