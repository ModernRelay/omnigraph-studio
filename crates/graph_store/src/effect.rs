//! What a branch does to the graph's own questions (spec 036): the reads a person marked for
//! a graph's reviews, each run at the target's head and at the branch's, with the rows that
//! came, went and moved; Studio's own checks; and the merge's result, held as an unpublished
//! commit (R2) for the checks to run on before it is published or dropped.
//!
//! One entity per reviewed pair of branches, shared through the session
//! ([`GraphSession::effect`]): the review's merge strip reads its checks, the Effect tab shows
//! all of it. Every read is a stored read the server runs, pinned to a commit; writes (the
//! unpublished merge, its publish and its drop) go through the session, sent once each.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use gpui::{App, AppContext as _, Context, Entity, Subscription, Task, WeakEntity};
use gq_language::builder;
use graph_client::api::{AccessAction, BranchMergeOutcome, BranchMergeOutput};
use graph_client::{ReadError, WriteOutcome};
use serde_json::Value;
use studio_settings::{EffectMark, StudioSettings};

use crate::access::{AccessScope, Allowed};
use crate::changes::endpoints_of;
use crate::row_diff::{RowDiff, cells, diff_rows};
use crate::session::{GraphSession, Loadable, SessionEvent};

/// How many reads are out at once.
pub const READS_AT_ONCE: usize = 4;
/// The most relationships a branch added that the built-in check looks at.
pub const EDGES_CHECKED: usize = 2_000;
/// A page of the branch's added relationships.
const EDGE_PAGE: u32 = 1_000;
/// The most pairs of ends one read of the target asks about.
const PAIRS_A_READ: usize = 500;
/// The most edges one read of the target answers.
const EDGES_A_READ: u32 = 5_000;
/// The key of the built-in check in the list of reads.
pub const DUPLICATE_EDGES: &str = "builtin:duplicate-edges";

/// The commits the Effect was read at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Heads {
    pub branch: String,
    pub target: String,
    /// The merge base: the target has moved since the fork when it is not `target`.
    pub base: String,
}

impl Heads {
    /// The target took commits since the branch forked from it, so a row the target gained
    /// since reads as removed by the branch.
    pub fn target_moved(&self) -> bool {
        self.base != self.target
    }
}

/// One side of a read: being read, its rows, or why it has none.
#[derive(Clone, Debug)]
pub enum Side {
    Reading,
    Rows(Arc<Rows>),
    Failed(String),
}

impl Side {
    pub fn rows(&self) -> Option<&Arc<Rows>> {
        match self {
            Side::Rows(rows) => Some(rows),
            _ => None,
        }
    }
}

/// A read's answer, its cells in its columns' order.
#[derive(Debug, PartialEq)]
pub struct Rows {
    pub columns: Vec<String>,
    pub cells: Vec<Vec<Value>>,
    pub row_count: usize,
    /// Only part of what there was to look at was read (the built-in check's cap).
    pub partial: Option<String>,
}

/// What a read runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// A stored read, with a saved view's parameters.
    Stored { name: String, params: Option<Value> },
    /// Studio's own check: the relationships the branch adds, of a type without a key, that
    /// the target already has between the same ends, or that the branch adds twice. A merge
    /// keeps both, so the graph then holds the relationship twice (dev D4).
    DuplicateEdges,
}

/// One read of the Effect.
#[derive(Clone, Debug)]
pub struct EffectRead {
    /// Its key in the graph's marks ([`studio_settings::GraphOverrides::effect`]).
    pub key: String,
    /// What the list calls it: the stored read's or the saved view's name.
    pub label: String,
    pub description: Option<String>,
    pub source: Source,
    pub mark: EffectMark,
    /// Why it is not run, in words (a parameter it needs, a stored query that writes, a view
    /// removed); `None` for one that runs.
    pub unrunnable: Option<String>,
    /// At the target's head; `None` until asked, and always for the built-in check.
    pub at_target: Option<Side>,
    pub at_branch: Option<Side>,
    /// At the unpublished merge, while one is held.
    pub at_result: Option<Side>,
    /// How the branch's rows differ from the target's.
    pub diff: Option<Arc<RowDiff>>,
    /// How the merge result's rows differ from the target's.
    pub result_diff: Option<Arc<RowDiff>>,
}

impl EffectRead {
    pub fn is_check(&self) -> bool {
        self.mark == EffectMark::Check
    }

    pub fn is_builtin(&self) -> bool {
        self.source == Source::DuplicateEdges
    }

    /// A check with rows at the branch head: what holds the merge.
    pub fn fails(&self) -> bool {
        self.is_check() && matches!(&self.at_branch, Some(Side::Rows(rows)) if rows.row_count > 0)
    }

    /// A check with rows at the merge result.
    pub fn fails_on_result(&self) -> bool {
        self.is_check() && matches!(&self.at_result, Some(Side::Rows(rows)) if rows.row_count > 0)
    }

    fn reading(&self) -> bool {
        [&self.at_target, &self.at_branch]
            .iter()
            .any(|side| matches!(side, Some(Side::Reading)))
    }
}

/// How much of the Effect a view wants read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Want {
    Nothing,
    /// The checks, for the review's merge.
    Checks,
    /// Everything, for the Effect tab.
    All,
}

/// What the checks say of a merge of the branch at a head.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Hold {
    /// No check, or none with rows.
    Clear,
    /// Not read at that head.
    Unread,
    /// Checks still being read.
    Reading(usize),
    /// Checks with rows at the branch head, by name.
    Rows(Vec<String>),
}

/// Which write of the merge result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResultStep {
    Merge,
    Publish,
    Drop,
}

impl ResultStep {
    pub fn verb(self) -> &'static str {
        match self {
            Self::Merge => "merge",
            Self::Publish => "publish",
            Self::Drop => "drop",
        }
    }
}

/// The merge's result held as an unpublished commit (R2), for the checks to run on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeResult {
    None,
    Merging,
    /// The unpublished merge commit. The target's head is where it was until it is published.
    Held {
        commit: String,
    },
    Publishing {
        commit: String,
    },
    Dropping {
        commit: String,
    },
    Published {
        commit: String,
    },
    Dropped,
    /// The branch is already in the target: there is no merge to check.
    AlreadyMerged,
    /// The server refused it: nothing was written. `commit` is the unpublished commit still
    /// held, when one is.
    Refused {
        step: ResultStep,
        reason: String,
        commit: Option<String>,
    },
    /// The answer was lost. Never sent again by itself (constitution III).
    Unknown {
        step: ResultStep,
        reason: String,
        commit: Option<String>,
    },
}

impl MergeResult {
    /// The unpublished commit this Effect holds, to publish or drop.
    pub fn held(&self) -> Option<&str> {
        match self {
            MergeResult::Held { commit }
            | MergeResult::Refused {
                step: ResultStep::Publish | ResultStep::Drop,
                commit: Some(commit),
                ..
            } => Some(commit),
            _ => None,
        }
    }

    /// A write of it is out, or its outcome is not known.
    pub fn busy(&self) -> bool {
        matches!(
            self,
            MergeResult::Merging
                | MergeResult::Publishing { .. }
                | MergeResult::Dropping { .. }
                | MergeResult::Unknown { .. }
        )
    }
}

/// The Effect of `branch` against `target`.
pub struct Effect {
    session: WeakEntity<GraphSession>,
    pub branch: String,
    pub target: String,
    want: Want,
    heads: Loadable<Heads>,
    /// A head moved since the Effect was read; the view offers Refresh (constitution II).
    pub moved: bool,
    reads: Vec<EffectRead>,
    merge_result: MergeResult,
    _heads_task: Option<Task<()>>,
    _reads_task: Option<Task<()>>,
    _result_reads_task: Option<Task<()>>,
    _edges_task: Option<Task<()>>,
    _write_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl Effect {
    pub fn new(
        session: Entity<GraphSession>,
        branch: String,
        target: String,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut subscriptions = vec![cx.subscribe(&session, |this, _, event, cx| match event {
            SessionEvent::HeadMoved { branch, head }
                if *branch == this.branch || *branch == this.target =>
            {
                let held = match this.heads.loaded() {
                    Some(heads) if *branch == this.branch => heads.branch != *head,
                    Some(heads) => heads.target != *head,
                    None => false,
                };
                if held && !this.moved {
                    this.moved = true;
                    cx.notify();
                }
            }
            // The schema says which relationships have no key, for Studio's own check.
            SessionEvent::StoredQueriesChanged | SessionEvent::SchemaChanged => this.resolve(cx),
            SessionEvent::CredentialChanged => this.refresh(cx),
            SessionEvent::AccessChanged => cx.notify(),
            _ => {}
        })];
        if cx.has_global::<StudioSettings>() {
            subscriptions.push(cx.observe_global::<StudioSettings>(|this, cx| this.resolve(cx)));
        }
        let effect = Self {
            session: session.downgrade(),
            branch,
            target,
            want: Want::Nothing,
            heads: Loadable::NotLoaded,
            moved: false,
            reads: Vec::new(),
            merge_result: MergeResult::None,
            _heads_task: None,
            _reads_task: None,
            _result_reads_task: None,
            _edges_task: None,
            _write_task: None,
            _subscriptions: subscriptions,
        };
        // Made inside the session's own update (`GraphSession::effect`), which this cannot
        // read: the marks are resolved once that is done.
        let this = cx.entity().downgrade();
        cx.defer(move |cx| {
            this.update(cx, |effect, cx| effect.resolve(cx)).ok();
        });
        effect
    }

    pub fn reads(&self) -> &[EffectRead] {
        &self.reads
    }

    pub fn heads(&self) -> &Loadable<Heads> {
        &self.heads
    }

    pub fn merge_result(&self) -> &MergeResult {
        &self.merge_result
    }

    /// The reads marked as checks, Studio's own first.
    pub fn checks(&self) -> impl Iterator<Item = &EffectRead> {
        self.reads.iter().filter(|read| read.is_check())
    }

    /// The reads marked as watched.
    pub fn watched(&self) -> impl Iterator<Item = &EffectRead> {
        self.reads.iter().filter(|read| !read.is_check())
    }

    /// Whether the person marked any read for this graph.
    pub fn has_marks(&self) -> bool {
        self.reads.iter().any(|read| !read.is_builtin())
    }

    /// Asks that what `want` covers be read at the heads now, unless it is: the review asks
    /// for the checks of the head it shows (`branch_head`), the Effect tab for everything.
    pub fn want(&mut self, want: Want, branch_head: Option<&str>, cx: &mut Context<Self>) {
        let more = want > self.want;
        self.want = self.want.max(want);
        // Nothing marked and nothing of Studio's own to check: nothing to read. A mark made
        // later reads then ([`Self::resolve`]).
        if self.reads.is_empty() {
            return;
        }
        let stale = match (self.heads.loaded(), branch_head) {
            (Some(heads), Some(head)) => heads.branch != head,
            _ => false,
        };
        match &self.heads {
            Loadable::NotLoaded | Loadable::Failed(_) => self.read_heads(cx),
            Loadable::Loading => {}
            Loadable::Loaded(_) if stale => self.read_heads(cx),
            Loadable::Loaded(_) if more => self.read_missing(cx),
            Loadable::Loaded(_) => {}
        }
    }

    /// Reads it all again from the heads now: the person's Refresh.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.want == Want::Nothing {
            self.heads = Loadable::NotLoaded;
            return;
        }
        self.read_heads(cx);
    }

    /// What the checks say of a merge of the branch at `branch_head`.
    pub fn hold(&self, branch_head: Option<&str>) -> Hold {
        let Some(heads) = self.heads.loaded() else {
            return if self.checks().next().is_none() {
                Hold::Clear
            } else {
                Hold::Unread
            };
        };
        if branch_head.is_some_and(|head| head != heads.branch) {
            return Hold::Unread;
        }
        let failing: Vec<String> = self
            .checks()
            .filter(|read| read.fails())
            .map(|read| read.label.clone())
            .collect();
        if !failing.is_empty() {
            return Hold::Rows(failing);
        }
        let reading = self
            .checks()
            .filter(|read| read.unrunnable.is_none())
            .filter(|read| read.at_branch.is_none() || read.reading())
            .count();
        if reading > 0 {
            Hold::Reading(reading)
        } else {
            Hold::Clear
        }
    }

    /// Why the merge result cannot be checked now, for the button's tooltip; `None` when it can.
    pub fn result_refusal(&self, cx: &App) -> Option<String> {
        let session = self.session.upgrade()?;
        let session = session.read(cx);
        let into = AccessScope::branch_into(&self.branch, &self.target);
        if session.allows(AccessAction::BranchMerge, &into) == Allowed::No {
            return Some(format!(
                "Your credential may not merge {} into {}",
                self.branch, self.target
            ));
        }
        // A result it could neither publish nor drop would stay on the server.
        if session.allows(AccessAction::CommitDrop, &AccessScope::branch(&self.target))
            == Allowed::No
        {
            return Some(format!(
                "Your credential may not drop an unpublished commit on {}, so a merge result \
                 Studio held could not be let go",
                self.target
            ));
        }
        if self.merge_result.busy() {
            return Some("The merge result's last write has not settled".to_owned());
        }
        if self.merge_result.held().is_some() {
            return Some("Publish or drop the merge result held now first".to_owned());
        }
        None
    }

    /// Why the held merge result cannot be published, for the button's tooltip.
    pub fn publish_refusal(&self, cx: &App) -> Option<String> {
        let session = self.session.upgrade()?;
        (session.read(cx).allows(
            AccessAction::CommitPublish,
            &AccessScope::branch(&self.target),
        ) == Allowed::No)
            .then(|| {
                format!(
                    "Your credential may not publish a commit onto {}",
                    self.target
                )
            })
    }

    /// The marks and the catalog as they are now, as the list of reads: a read kept keeps
    /// what it read; a new one is read when wanted.
    fn resolve(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.upgrade() else {
            return;
        };
        let (graph, catalog, has_unkeyed) = {
            let session = session.read(cx);
            let catalog = session.stored_queries.loaded().cloned();
            let has_unkeyed = session
                .schema_model()
                .is_some_and(|schema| schema.edges.iter().any(|edge| !edge.keyed));
            (session.overrides_key(), catalog, has_unkeyed)
        };
        let (marks, views) = match cx.try_global::<StudioSettings>() {
            Some(settings) => (
                settings.effect_marks(&graph),
                settings.graph_views(&graph).to_vec(),
            ),
            None => (BTreeMap::new(), Vec::new()),
        };
        let mut reads = Vec::new();
        if has_unkeyed {
            reads.push(EffectRead {
                key: DUPLICATE_EDGES.to_owned(),
                label: format!("Relationship already on {}", self.target),
                description: Some(format!(
                    "Relationships this branch adds, of a type without a key, that {} already \
                     has between the same two nodes, or that the branch adds twice. A merge \
                     keeps both.",
                    self.target
                )),
                source: Source::DuplicateEdges,
                mark: EffectMark::Check,
                unrunnable: None,
                at_target: None,
                at_branch: None,
                at_result: None,
                diff: None,
                result_diff: None,
            });
        }
        for (key, mark) in marks {
            let (label, description, source, unrunnable) = match key.strip_prefix("view:") {
                Some(id) => match views.iter().find(|view| view.key() == id) {
                    Some(view) => match &view.query {
                        Some(query) if !query.stored.is_empty() => (
                            view.name.clone(),
                            Some(format!("{}, as the view keeps it", query.stored)),
                            Source::Stored {
                                name: query.stored.clone(),
                                params: (!query.params.is_empty())
                                    .then(|| Value::Object(query.params.clone())),
                            },
                            None,
                        ),
                        _ => (
                            view.name.clone(),
                            None,
                            Source::Stored {
                                name: String::new(),
                                params: None,
                            },
                            Some("Only a view of a stored query runs here".to_owned()),
                        ),
                    },
                    None => continue,
                },
                None => {
                    let entry = catalog
                        .as_ref()
                        .and_then(|catalog| catalog.iter().find(|q| q.name == key));
                    let unrunnable = match (&catalog, entry) {
                        (None, _) => None,
                        (Some(_), None) => {
                            Some("The graph no longer serves this stored query".to_owned())
                        }
                        (Some(_), Some(entry)) if entry.mutation => {
                            Some("It changes data, so it is not run here".to_owned())
                        }
                        (Some(_), Some(entry))
                            if entry.params.iter().any(|param| !param.nullable) =>
                        {
                            Some(
                                "It takes parameters: save a view of it with their values, \
                                     and mark the view"
                                    .to_owned(),
                            )
                        }
                        _ => None,
                    };
                    (
                        key.clone(),
                        entry.and_then(|entry| entry.description.clone()),
                        Source::Stored {
                            name: key.clone(),
                            params: None,
                        },
                        unrunnable,
                    )
                }
            };
            reads.push(EffectRead {
                key,
                label,
                description,
                source,
                mark,
                unrunnable,
                at_target: None,
                at_branch: None,
                at_result: None,
                diff: None,
                result_diff: None,
            });
        }
        // Keep what was read for a read that runs the same.
        let mut kept: HashMap<String, EffectRead> = std::mem::take(&mut self.reads)
            .into_iter()
            .map(|read| (read.key.clone(), read))
            .collect();
        for read in &mut reads {
            if let Some(old) = kept.remove(&read.key)
                && old.source == read.source
                && old.unrunnable.is_none()
                && read.unrunnable.is_none()
            {
                read.at_target = old.at_target;
                read.at_branch = old.at_branch;
                read.at_result = old.at_result;
                read.diff = old.diff;
                read.result_diff = old.result_diff;
            }
        }
        // Checks first, Studio's own leading them, then by name.
        reads.sort_by(|a, b| {
            (!a.is_check(), !a.is_builtin(), a.label.to_lowercase()).cmp(&(
                !b.is_check(),
                !b.is_builtin(),
                b.label.to_lowercase(),
            ))
        });
        self.reads = reads;
        match &self.heads {
            Loadable::Loaded(_) => self.read_missing(cx),
            Loadable::NotLoaded if !self.reads.is_empty() && self.want != Want::Nothing => {
                self.read_heads(cx)
            }
            _ => {}
        }
        cx.notify();
    }

    /// Reads both heads and the merge base, then every wanted read at them.
    fn read_heads(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.upgrade() else {
            return;
        };
        self.heads = Loadable::Loading;
        self.moved = false;
        self._reads_task = None;
        self._edges_task = None;
        for read in &mut self.reads {
            read.at_target = None;
            read.at_branch = None;
            read.diff = None;
            read.result_diff = None;
        }
        cx.notify();
        let read = session.update(cx, |session, cx| {
            session.merge_base(&self.branch, &self.target, cx)
        });
        self._heads_task = Some(cx.spawn(async move |this, cx| {
            let heads = match read.await {
                Ok(base) => Loadable::Loaded(Heads {
                    branch: base.source_commit_id,
                    target: base.target_commit_id,
                    base: base.merge_base.graph_commit_id,
                }),
                Err(error) => Loadable::Failed(error.to_string()),
            };
            this.update(cx, |this, cx| {
                this.heads = heads;
                this.read_missing(cx);
                cx.notify();
            })
            .ok();
        }));
    }

    /// Starts every wanted read not read at the heads held, at most [`READS_AT_ONCE`] out.
    fn read_missing(&mut self, cx: &mut Context<Self>) {
        let (Some(heads), Some(session)) = (self.heads.loaded().cloned(), self.session.upgrade())
        else {
            return;
        };
        let want = self.want;
        let mut jobs: Vec<(String, Source, String, bool)> = Vec::new();
        let mut edges = false;
        for read in &mut self.reads {
            let wanted = match want {
                Want::Nothing => false,
                Want::Checks => read.is_check(),
                Want::All => true,
            };
            if !wanted || read.unrunnable.is_some() {
                continue;
            }
            if read.is_builtin() {
                if read.at_branch.is_none() {
                    read.at_branch = Some(Side::Reading);
                    edges = true;
                }
                continue;
            }
            for (side, at_branch) in [(&mut read.at_target, false), (&mut read.at_branch, true)] {
                if side.is_none() || matches!(side, Some(Side::Reading)) {
                    *side = Some(Side::Reading);
                    let commit = if at_branch {
                        heads.branch.clone()
                    } else {
                        heads.target.clone()
                    };
                    jobs.push((read.key.clone(), read.source.clone(), commit, at_branch));
                }
            }
        }
        if edges {
            self.check_edges(&heads, cx);
        }
        if jobs.is_empty() {
            cx.notify();
            return;
        }
        cx.notify();
        let reads_at = heads.clone();
        self._reads_task = Some(cx.spawn(async move |this, cx| {
            for chunk in jobs.chunks(READS_AT_ONCE) {
                let tasks: Vec<_> = chunk
                    .iter()
                    .map(|(_, source, commit, _)| {
                        let Source::Stored { name, params } = source else {
                            unreachable!("only stored reads are jobs");
                        };
                        session.update(cx, |session, cx| {
                            session.invoke_stored_read(
                                name,
                                params.clone(),
                                Some(commit.clone()),
                                cx,
                            )
                        })
                    })
                    .collect();
                let answers = futures::future::join_all(tasks).await;
                let sides: Vec<(String, bool, Side)> = chunk
                    .iter()
                    .zip(answers)
                    .map(|((key, _, _, at_branch), answer)| {
                        (key.clone(), *at_branch, side_of(answer))
                    })
                    .collect();
                let Ok(()) = this.update(cx, |this, cx| {
                    if this.heads.loaded() != Some(&reads_at) {
                        return;
                    }
                    for (key, at_branch, side) in sides {
                        if let Some(read) = this.reads.iter_mut().find(|read| read.key == key) {
                            if at_branch {
                                read.at_branch = Some(side);
                            } else {
                                read.at_target = Some(side);
                            }
                        }
                    }
                    this.compare(cx);
                    cx.notify();
                }) else {
                    return;
                };
            }
        }));
    }

    /// Works out the row diffs of the reads whose two sides are both read, off the main thread.
    fn compare(&mut self, cx: &mut Context<Self>) {
        let mut pairs = Vec::new();
        for read in &self.reads {
            let target = read.at_target.as_ref().and_then(Side::rows);
            if let (Some(target), Some(branch), None) = (
                target,
                read.at_branch.as_ref().and_then(Side::rows),
                &read.diff,
            ) {
                pairs.push((read.key.clone(), false, target.clone(), branch.clone()));
            }
            if let (Some(target), Some(result), None) = (
                target,
                read.at_result.as_ref().and_then(Side::rows),
                &read.result_diff,
            ) {
                pairs.push((read.key.clone(), true, target.clone(), result.clone()));
            }
        }
        if pairs.is_empty() {
            return;
        }
        let work = cx.background_spawn(async move {
            pairs
                .into_iter()
                .map(|(key, result, before, after)| {
                    let diff = if before.columns == after.columns {
                        diff_rows(&before.columns, &before.cells, &after.cells)
                    } else {
                        // Another schema between the heads: no row is the same row.
                        RowDiff {
                            matched_by: crate::row_diff::MatchedBy::WholeRow,
                            added: (0..after.cells.len()).collect(),
                            removed: (0..before.cells.len()).collect(),
                            changed: Vec::new(),
                            capped: false,
                        }
                    };
                    (key, result, Arc::new(diff))
                })
                .collect::<Vec<_>>()
        });
        cx.spawn(async move |this, cx| {
            let diffs = work.await;
            this.update(cx, |this, cx| {
                for (key, result, diff) in diffs {
                    if let Some(read) = this.reads.iter_mut().find(|read| read.key == key) {
                        if result {
                            read.result_diff = Some(diff);
                        } else {
                            read.diff = Some(diff);
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Studio's own check ([`Source::DuplicateEdges`]): pages through the relationships the
    /// branch added since the fork, then asks the target's head for those of the types without
    /// a key between the same ends.
    fn check_edges(&mut self, heads: &Heads, cx: &mut Context<Self>) {
        let Some(session) = self.session.upgrade() else {
            return;
        };
        let Some(schema) = session.read(cx).schema_model() else {
            return;
        };
        let heads = heads.clone();
        let target = self.target.clone();
        self._edges_task = Some(cx.spawn(async move |this, cx| {
            let side = match duplicate_edges(&session, &schema, &heads, &target, cx).await {
                Ok(rows) => Side::Rows(Arc::new(rows)),
                Err(error) => Side::Failed(error),
            };
            this.update(cx, |this, cx| {
                if this.heads.loaded() != Some(&heads) {
                    return;
                }
                if let Some(read) = this.reads.iter_mut().find(|read| read.is_builtin()) {
                    read.at_branch = Some(side);
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Merges the branch into the target as an unpublished commit, and runs the reads at it.
    /// Sent once; the target's head does not move.
    pub fn check_merge_result(&mut self, cx: &mut Context<Self>) {
        if self.result_refusal(cx).is_some() {
            return;
        }
        let Some(session) = self.session.upgrade() else {
            return;
        };
        self.merge_result = MergeResult::Merging;
        for read in &mut self.reads {
            read.at_result = None;
            read.result_diff = None;
        }
        cx.notify();
        let write = session.update(cx, |session, cx| {
            session.merge_as_commit(self.branch.clone(), self.target.clone(), cx)
        });
        self._write_task = Some(cx.spawn(async move |this, cx| {
            let outcome = write.await;
            this.update(cx, |this, cx| {
                this.merge_result = match outcome {
                    WriteOutcome::Committed(BranchMergeOutput {
                        outcome: BranchMergeOutcome::AlreadyUpToDate,
                        ..
                    }) => MergeResult::AlreadyMerged,
                    WriteOutcome::Committed(BranchMergeOutput {
                        commit: Some(commit),
                        ..
                    }) => MergeResult::Held {
                        commit: commit.graph_commit_id,
                    },
                    WriteOutcome::Committed(_) => MergeResult::Unknown {
                        step: ResultStep::Merge,
                        reason: "the server named no commit".to_owned(),
                        commit: None,
                    },
                    WriteOutcome::Refused(error) => MergeResult::Refused {
                        step: ResultStep::Merge,
                        reason: error.reason().to_owned(),
                        commit: None,
                    },
                    WriteOutcome::NotSent { reason } => MergeResult::Refused {
                        step: ResultStep::Merge,
                        reason: format!("not sent: {reason}"),
                        commit: None,
                    },
                    WriteOutcome::Unknown { reason } => MergeResult::Unknown {
                        step: ResultStep::Merge,
                        reason,
                        commit: None,
                    },
                };
                this.read_result(cx);
                cx.notify();
            })
            .ok();
        }));
    }

    /// Runs every runnable stored read at the held merge result.
    fn read_result(&mut self, cx: &mut Context<Self>) {
        let (Some(commit), Some(session)) = (
            self.merge_result.held().map(str::to_owned),
            self.session.upgrade(),
        ) else {
            return;
        };
        let mut jobs = Vec::new();
        for read in &mut self.reads {
            if read.unrunnable.is_some() || read.is_builtin() {
                continue;
            }
            read.at_result = Some(Side::Reading);
            read.result_diff = None;
            jobs.push((read.key.clone(), read.source.clone()));
        }
        // The reads at the target head are compared with it.
        self.want = Want::All;
        self.read_missing(cx);
        self._result_reads_task = Some(cx.spawn(async move |this, cx| {
            for chunk in jobs.chunks(READS_AT_ONCE) {
                let tasks: Vec<_> = chunk
                    .iter()
                    .map(|(_, source)| {
                        let Source::Stored { name, params } = source else {
                            unreachable!("only stored reads run at the result");
                        };
                        session.update(cx, |session, cx| {
                            session.invoke_stored_read(
                                name,
                                params.clone(),
                                Some(commit.clone()),
                                cx,
                            )
                        })
                    })
                    .collect();
                let answers = futures::future::join_all(tasks).await;
                let Ok(()) = this.update(cx, |this, cx| {
                    for ((key, _), answer) in chunk.iter().zip(answers) {
                        if let Some(read) = this.reads.iter_mut().find(|read| read.key == *key) {
                            read.at_result = Some(side_of(answer));
                        }
                    }
                    this.compare(cx);
                    cx.notify();
                }) else {
                    return;
                };
            }
        }));
    }

    /// Publishes the held merge result onto the target, which moves its head. Sent once; a
    /// refusal (the target moved since, so the commit expired) keeps it held, to drop.
    pub fn publish(&mut self, cx: &mut Context<Self>) {
        let (Some(commit), Some(session)) = (
            self.merge_result.held().map(str::to_owned),
            self.session.upgrade(),
        ) else {
            return;
        };
        if self.publish_refusal(cx).is_some() {
            return;
        }
        self.merge_result = MergeResult::Publishing {
            commit: commit.clone(),
        };
        cx.notify();
        let write = session.update(cx, |session, cx| {
            session.publish_commit(commit.clone(), self.target.clone(), cx)
        });
        self._write_task = Some(cx.spawn(async move |this, cx| {
            let outcome = write.await;
            this.update(cx, |this, cx| {
                this.merge_result = match outcome {
                    WriteOutcome::Committed(published) => MergeResult::Published {
                        commit: published.graph_commit_id,
                    },
                    WriteOutcome::Refused(error) => MergeResult::Refused {
                        step: ResultStep::Publish,
                        reason: error.reason().to_owned(),
                        commit: Some(commit),
                    },
                    WriteOutcome::NotSent { reason } => MergeResult::Refused {
                        step: ResultStep::Publish,
                        reason: format!("not sent: {reason}"),
                        commit: Some(commit),
                    },
                    WriteOutcome::Unknown { reason } => MergeResult::Unknown {
                        step: ResultStep::Publish,
                        reason,
                        commit: Some(commit),
                    },
                };
                cx.notify();
            })
            .ok();
        }));
    }

    /// Drops the held merge result, which no branch holds. Sent once.
    pub fn drop_result(&mut self, cx: &mut Context<Self>) {
        let (Some(commit), Some(session)) = (
            self.merge_result.held().map(str::to_owned),
            self.session.upgrade(),
        ) else {
            return;
        };
        self.merge_result = MergeResult::Dropping {
            commit: commit.clone(),
        };
        cx.notify();
        let write = session.update(cx, |session, cx| {
            session.drop_commit(commit.clone(), self.target.clone(), cx)
        });
        self._write_task = Some(cx.spawn(async move |this, cx| {
            let outcome = write.await;
            this.update(cx, |this, cx| {
                this.merge_result = match outcome {
                    WriteOutcome::Committed(()) => {
                        for read in &mut this.reads {
                            read.at_result = None;
                            read.result_diff = None;
                        }
                        MergeResult::Dropped
                    }
                    WriteOutcome::Refused(error) => MergeResult::Refused {
                        step: ResultStep::Drop,
                        reason: error.reason().to_owned(),
                        commit: Some(commit),
                    },
                    WriteOutcome::NotSent { reason } => MergeResult::Refused {
                        step: ResultStep::Drop,
                        reason: format!("not sent: {reason}"),
                        commit: Some(commit),
                    },
                    WriteOutcome::Unknown { reason } => MergeResult::Unknown {
                        step: ResultStep::Drop,
                        reason,
                        commit: Some(commit),
                    },
                };
                cx.notify();
            })
            .ok();
        }));
    }
}

/// A stored read's answer as one side of a read.
fn side_of(answer: Result<graph_client::api::ReadOutput, ReadError>) -> Side {
    match answer {
        Ok(output) => {
            let cells = cells(&output.columns, &output.rows);
            Side::Rows(Arc::new(Rows {
                row_count: cells.len(),
                columns: output.columns,
                cells,
                partial: None,
            }))
        }
        Err(ReadError::TooLarge { .. }) => {
            Side::Failed("Too large to compare: the answer is over Studio's limit".to_owned())
        }
        Err(error) => Side::Failed(error.to_string()),
    }
}

/// One relationship the branch added.
struct Added {
    type_name: String,
    from: String,
    to: String,
}

/// The relationships the branch added, of types without a key, that the target has between
/// the same ends or that the branch added twice, as rows: `relationship`, `from`, `to`,
/// `already`.
async fn duplicate_edges(
    session: &Entity<GraphSession>,
    schema: &gq_language::SchemaModel,
    heads: &Heads,
    target: &str,
    cx: &mut gpui::AsyncApp,
) -> Result<Rows, String> {
    let mut added: Vec<Added> = Vec::new();
    let mut token: Option<String> = None;
    let mut partial = None;
    loop {
        let page = session.update(cx, |session, cx| {
            session.inserted_edges(&heads.base, &heads.branch, token.clone(), EDGE_PAGE, cx)
        });
        let page = page.await.map_err(|error| error.to_string())?;
        for change in page.changes {
            let unkeyed = schema
                .edge(&change.entity_type.name)
                .is_some_and(|edge| !edge.keyed);
            let ends = change
                .after
                .as_ref()
                .and_then(|after| after.endpoints.as_ref())
                .and_then(endpoints_of);
            if let (true, Some((from, to))) = (unkeyed, ends) {
                added.push(Added {
                    type_name: change.entity_type.name.clone(),
                    from,
                    to,
                });
            }
        }
        token = page.next_page_token;
        if added.len() >= EDGES_CHECKED && token.is_some() {
            added.truncate(EDGES_CHECKED);
            partial = Some(format!(
                "Studio checked the first {} relationships the branch added",
                EDGES_CHECKED
            ));
            break;
        }
        if token.is_none() {
            break;
        }
    }
    // How often the branch adds each pair, and whether the target has it.
    let mut twice: HashMap<(String, String, String), usize> = HashMap::new();
    for edge in &added {
        *twice
            .entry((edge.type_name.clone(), edge.from.clone(), edge.to.clone()))
            .or_default() += 1;
    }
    let mut by_type: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for (type_name, from, to) in twice.keys() {
        by_type
            .entry(type_name.clone())
            .or_default()
            .push((from.clone(), to.clone()));
    }
    let mut on_target: std::collections::HashSet<(String, String, String)> = Default::default();
    for (type_name, pairs) in by_type {
        for chunk in pairs.chunks(PAIRS_A_READ) {
            let mut from: Vec<String> = chunk.iter().map(|(from, _)| from.clone()).collect();
            let mut to: Vec<String> = chunk.iter().map(|(_, to)| to.clone()).collect();
            from.sort();
            from.dedup();
            to.sort();
            to.dedup();
            let query = builder::edges_between(schema, &type_name, &from, &to, EDGES_A_READ)
                .map_err(|error| error.to_string())?;
            let read = session.update(cx, |session, cx| {
                session.read(&query, Some(heads.target.clone()), cx)
            });
            let output = read.await.map_err(|error| error.to_string())?;
            for row in output.rows.as_array().into_iter().flatten() {
                let text = |key: &str| row.get(key).and_then(Value::as_str).map(str::to_owned);
                if let (Some(from), Some(to)) = (text("from_id"), text("to_id")) {
                    on_target.insert((type_name.clone(), from, to));
                }
            }
        }
    }
    let mut keys: Vec<_> = twice.into_iter().collect();
    keys.sort();
    let cells: Vec<Vec<Value>> = keys
        .into_iter()
        .filter_map(|(key, count)| {
            let already = on_target.contains(&key);
            let why = match (already, count > 1) {
                (true, true) => format!("on {target}, and added {count} times here"),
                (true, false) => format!("on {target}"),
                (false, true) => format!("added {count} times here"),
                (false, false) => return None,
            };
            let (type_name, from, to) = key;
            Some(vec![
                Value::String(type_name),
                Value::String(from),
                Value::String(to),
                Value::String(why),
            ])
        })
        .collect();
    Ok(Rows {
        columns: ["relationship", "from", "to", "already"]
            .map(str::to_owned)
            .to_vec(),
        row_count: cells.len(),
        cells,
        partial,
    })
}
