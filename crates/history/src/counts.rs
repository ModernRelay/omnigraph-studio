//! The change counts beside a listed commit (spec 009), as History and the Commits panel
//! (spec 026) show them: one page of the commit's changes each, read for the rows drawn once
//! scrolling pauses, and kept, since a commit's changes never change.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use change_cards::CommitCounts;
use gpui::{AppContext as _, Context, Entity, Task};
use graph_client::ReadError;
use graph_client::api::{ChangeDiffRefusalReason, CommitOutput, EntityChangeOutput, EntityKind};
use graph_store::{GraphSession, NetChange, TypeCounts};

/// Changes read per commit for a row's counts: each comes with its before and after images, and a
/// drawn row asks for its own. A commit with more shows its counts as partial.
pub(crate) const ROW_COUNT_PAGE: u32 = 100;

/// The pause in scrolling before the drawn rows' counts are read.
pub(crate) const COUNTS_AFTER: Duration = Duration::from_millis(120);

/// Added, changed and removed entities.
#[cfg(test)]
pub(crate) type Counts = (usize, usize, usize);

/// What a view has learnt of a listed commit's changes (spec 033): its counts as every list of
/// commits says them, and the same by type, node types first, for the inspector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Learnt {
    pub(crate) counts: CommitCounts,
    pub(crate) by_type: Arc<[TypeCounts]>,
    /// The nodes the page changed, or the ends of the relationships it changed when it changed
    /// no node, the first [`TOUCHED_KEPT`] of them; what a row says the commit was about, and
    /// what a search for a node's id finds (spec 040).
    pub(crate) touched: Arc<[Touched]>,
    /// How many nodes the page touched in all.
    pub(crate) touched_count: usize,
}

/// A node a commit changed, as its row names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Touched {
    pub(crate) type_name: Option<String>,
    pub(crate) id: String,
}

/// Nodes a listed commit keeps by name: a row names one, and a search matches the rest.
pub(crate) const TOUCHED_KEPT: usize = 24;

impl Learnt {
    /// What one page of a commit's changes says: counted by type, and in all, partial when
    /// more pages follow it.
    fn counted(changes: &[EntityChangeOutput], partial: bool) -> Self {
        let by_type = tally_by_type(changes);
        let (added, changed, removed) = by_type.iter().fold((0, 0, 0), |(a, c, r), t| {
            (a + t.added, c + t.changed, r + t.removed)
        });
        let (touched, touched_count) = touched_by(changes);
        Learnt {
            counts: CommitCounts::Counted {
                added,
                changed,
                removed,
                partial,
            },
            by_type: by_type.into(),
            touched: touched.into(),
            touched_count,
        }
    }

    /// What the commit was about, in a few words (spec 040): the node it changed ("Issue
    /// lk-acl"), the first of several ("Issue lk-acl and 2 more"), or none.
    pub(crate) fn about(&self) -> Option<String> {
        let first = self.touched.first()?;
        let named = match &first.type_name {
            Some(type_name) => format!("{type_name} {}", first.id),
            None => first.id.clone(),
        };
        Some(match self.touched_count {
            0 | 1 => named,
            n => format!("{named} and {} more", n - 1),
        })
    }

    /// Whether the commit touched a node whose id holds `query` (lowercase).
    pub(crate) fn touches(&self, query: &str) -> bool {
        self.touched
            .iter()
            .any(|touched| touched.id.to_lowercase().contains(query))
    }

    /// What a lasting refusal, or the graph's first commit, says in place of counts.
    fn said(counts: CommitCounts) -> Self {
        Learnt {
            counts,
            by_type: Arc::from([]),
            touched: Arc::from([]),
            touched_count: 0,
        }
    }

    /// The counts, and whether they are the first page's only, when there are counts.
    #[cfg(test)]
    pub(crate) fn totals(&self) -> Option<(Counts, bool)> {
        match self.counts {
            CommitCounts::Counted {
                added,
                changed,
                removed,
                partial,
            } => Some(((added, changed, removed), partial)),
            _ => None,
        }
    }
}

/// The nodes a page of changes touched, in the order they come, the first [`TOUCHED_KEPT`]
/// kept, and how many a row names: the nodes it changed first, then the ends of the
/// relationships it changed, which name no type. A row names the nodes, or the ends when it
/// changed no node; a search finds either.
fn touched_by(changes: &[EntityChangeOutput]) -> (Vec<Touched>, usize) {
    fn take<'a>(
        seen: &mut HashSet<&'a str>,
        kept: &mut Vec<Touched>,
        type_name: Option<&'a str>,
        id: &'a str,
    ) {
        if seen.insert(id) && kept.len() < TOUCHED_KEPT {
            kept.push(Touched {
                type_name: type_name.map(str::to_owned),
                id: id.to_owned(),
            });
        }
    }
    let mut seen = HashSet::new();
    let mut kept = Vec::new();
    for change in changes {
        if matches!(change.kind, EntityKind::Node) {
            take(
                &mut seen,
                &mut kept,
                Some(&change.entity_type.name),
                &change.id,
            );
        }
    }
    let nodes = seen.len();
    // Both images': a relationship moved from one node to another touched both.
    for change in changes {
        for image in [change.after.as_ref(), change.before.as_ref()]
            .into_iter()
            .flatten()
        {
            let ends = image
                .endpoints
                .as_ref()
                .and_then(|ends| Some((ends.get("from")?.as_str()?, ends.get("to")?.as_str()?)));
            if let Some((from, to)) = ends {
                take(&mut seen, &mut kept, None, from);
                take(&mut seen, &mut kept, None, to);
            }
        }
    }
    let named = if nodes > 0 { nodes } else { seen.len() };
    (kept, named)
}

/// A page's changes counted by type, in the order they come, node types before relationships'.
pub(crate) fn tally_by_type(changes: &[EntityChangeOutput]) -> Vec<TypeCounts> {
    let mut counts: Vec<TypeCounts> = Vec::new();
    let mut index: HashMap<(bool, &str), usize> = HashMap::new();
    for change in changes {
        let is_edge = matches!(change.kind, EntityKind::Edge);
        let ix = *index
            .entry((is_edge, change.entity_type.id.as_str()))
            .or_insert_with(|| {
                counts.push(TypeCounts {
                    is_edge,
                    type_name: change.entity_type.name.clone(),
                    added: 0,
                    changed: 0,
                    removed: 0,
                });
                counts.len() - 1
            });
        match NetChange::of(change.before.as_ref(), change.after.as_ref()) {
            Some(NetChange::Added) => counts[ix].added += 1,
            Some(NetChange::Changed) => counts[ix].changed += 1,
            Some(NetChange::Removed) => counts[ix].removed += 1,
            None => {}
        }
    }
    counts.sort_by_key(|c| c.is_edge);
    counts
}

/// What a refusal of a commit's changes says of the commit for good, the one reading of it
/// that a commit's tab and every list of commits share (spec 033): the graph's first commit
/// (409, `parentless_commit`); a schema change (409, `schema_boundary`, at the type it names);
/// another comparison the server refuses, as it says it (409); history cleaned up (410).
/// `None` for a failure that may pass (a timeout, a 5xx, a 429), which is asked again.
pub(crate) fn lasting(error: &ReadError) -> Option<CommitCounts> {
    let ReadError::Refused(refusal) = error else {
        return None;
    };
    match refusal.status {
        410 => Some(CommitCounts::CleanedUp),
        409 => Some(
            match refusal
                .change_diff_refusal
                .as_ref()
                .map(|diff| (diff.reason, diff.type_name.clone()))
            {
                Some((ChangeDiffRefusalReason::ParentlessCommit, _)) => CommitCounts::First,
                Some((ChangeDiffRefusalReason::SchemaBoundary, type_name)) => {
                    CommitCounts::SchemaChange(type_name)
                }
                _ => CommitCounts::NotComparable(refusal.message.clone()),
            },
        ),
        _ => None,
    }
}

/// The counts read for a view's commits, and the reads under way.
#[derive(Default)]
pub(crate) struct RowCounts {
    /// What is known of each commit's changes, by its id: its counts, or the lasting refusal
    /// said in their place. One fact a commit, so a commit is never both.
    pub(crate) learnt: HashMap<String, Learnt>,
    /// The commits asked for already.
    asked: HashSet<String>,
    /// The reads under way, by commit; and the commits drawn they were last asked for, with
    /// the pause before they are read.
    tasks: HashMap<String, Task<()>>,
    drawn: Option<Vec<String>>,
    _wait: Option<Task<()>>,
}

impl RowCounts {
    /// Takes `counts` as commit `id`'s, as a read of its first page would, for a test.
    #[cfg(test)]
    pub(crate) fn insert_counted(&mut self, id: &str, (added, changed, removed): Counts) {
        self.learnt.insert(
            id.to_owned(),
            Learnt::said(CommitCounts::Counted {
                added,
                changed,
                removed,
                partial: false,
            }),
        );
    }

    /// What is known of `commit`'s changes: the graph's first commit has none to count; one
    /// the server refused for good says why; one counted says its counts. None while it is
    /// not read.
    pub(crate) fn learnt(&self, commit: &CommitOutput) -> Option<Learnt> {
        if crate::commit_view::is_first_commit(commit) {
            return Some(Learnt::said(CommitCounts::First));
        }
        self.learnt.get(&commit.graph_commit_id).cloned()
    }

    /// The rows drawn are asked again: what is waiting for them is not dropped.
    pub(crate) fn forget_drawn(&mut self) {
        self.drawn = None;
    }

    /// Asks for the counts of the commits `drawn` once scrolling has paused: a fast scroll reads
    /// nothing for the rows it passes, and reads still out for rows no longer drawn are dropped
    /// (and asked again if they come back). `of` finds the counts in the view.
    pub(crate) fn want<V: 'static>(
        &mut self,
        session: &Entity<GraphSession>,
        drawn: Vec<&CommitOutput>,
        of: fn(&mut V) -> &mut RowCounts,
        cx: &mut Context<V>,
    ) {
        let ids: Vec<String> = drawn
            .iter()
            .map(|commit| commit.graph_commit_id.clone())
            .collect();
        if self.drawn.as_ref() == Some(&ids) {
            return;
        }
        self.drawn = Some(ids.clone());
        // The graph's first commit has no parent to compare with, so its changes are not
        // asked for.
        let wanted: Vec<String> = drawn
            .into_iter()
            .filter(|commit| !crate::commit_view::is_first_commit(commit))
            .map(|commit| commit.graph_commit_id.clone())
            .collect();
        let session = session.clone();
        self._wait = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(COUNTS_AFTER).await;
            this.update(cx, |this, cx| {
                let counts = of(this);
                let shown: HashSet<&String> = ids.iter().collect();
                let gone: Vec<String> = counts
                    .tasks
                    .keys()
                    .filter(|id| !shown.contains(id))
                    .cloned()
                    .collect();
                for id in gone {
                    counts.tasks.remove(&id);
                    counts.asked.remove(&id);
                }
                counts.read(&session, wanted, of, cx);
            })
            .ok();
        }));
    }

    /// Reads change counts for `ids` that have none yet: one page each, netted as the detail
    /// nets them. A commit with more changes than the page shows "N+". An answer is kept for
    /// the life of the view.
    pub(crate) fn read<V: 'static>(
        &mut self,
        session: &Entity<GraphSession>,
        ids: Vec<String>,
        of: fn(&mut V) -> &mut RowCounts,
        cx: &mut Context<V>,
    ) {
        let ids: Vec<String> = ids
            .into_iter()
            .filter(|id| !self.learnt.contains_key(id) && !self.asked.contains(id))
            .collect();
        for id in ids {
            self.asked.insert(id.clone());
            let key = id.clone();
            let read = session.update(cx, |session, cx| {
                session.commit_changes(&id, None, ROW_COUNT_PAGE, cx)
            });
            let task = cx.spawn(async move |this, cx| {
                let page = match read.await {
                    Ok(page) => page,
                    Err(error) => {
                        // A lasting refusal (the first commit, a schema change, another
                        // comparison refused, history cleaned up) is said where the counts
                        // would be, and the commit stays asked, so a row scrolled back to does
                        // not ask again. Another failure may pass, and is asked again.
                        let said = lasting(&error);
                        this.update(cx, |this, cx| {
                            let counts = of(this);
                            counts.tasks.remove(&id);
                            match said {
                                Some(said) => {
                                    counts.learnt.insert(id.clone(), Learnt::said(said));
                                    cx.notify();
                                }
                                None => {
                                    counts.asked.remove(&id);
                                }
                            }
                        })
                        .ok();
                        return;
                    }
                };
                let more = page.next_page_token.is_some();
                let learnt = cx
                    .background_spawn(async move { Learnt::counted(&page.changes, more) })
                    .await;
                this.update(cx, |this, cx| {
                    let counts = of(this);
                    counts.tasks.remove(&id);
                    counts.learnt.insert(id.clone(), learnt);
                    cx.notify();
                })
                .ok();
            });
            self.tasks.insert(key, task);
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn change(value: serde_json::Value) -> EntityChangeOutput {
        serde_json::from_value(value).unwrap()
    }

    /// A commit's row names the node it changed, or the first of several; a commit that changed
    /// only relationships names their ends. A search finds a commit by a node it touched (spec
    /// 040; beads B2: "lk-acl" found nothing).
    #[test]
    fn a_counted_commit_names_the_nodes_it_touched() {
        let node = |id: &str| {
            change(
                json!({ "kind": "node", "type": { "id": "t1", "name": "Issue" }, "id": id,
                "op": "update", "before": { "properties": { "status": "open" } },
                "after": { "properties": { "status": "closed" } } }),
            )
        };
        let one = Learnt::counted(&[node("lk-acl")], false);
        assert_eq!(one.about().as_deref(), Some("Issue lk-acl"));
        assert!(one.touches("lk-acl") && one.touches("acl") && !one.touches("lk-share"));
        let three = Learnt::counted(&[node("lk-acl"), node("lk-share"), node("lk-acl")], false);
        assert_eq!(three.about().as_deref(), Some("Issue lk-acl and 1 more"));
        let edge = change(
            json!({ "kind": "edge", "type": { "id": "t2", "name": "CommentOn" },
            "id": "e1", "op": "insert",
            "after": { "properties": {}, "endpoints": { "from": "c-1", "to": "lk-acl" } } }),
        );
        let linked = Learnt::counted(std::slice::from_ref(&edge), false);
        assert_eq!(linked.about().as_deref(), Some("c-1 and 1 more"));
        assert!(linked.touches("lk-acl"));
        // A comment added and linked: the row names the comment, a search finds the issue.
        let comment = change(
            json!({ "kind": "node", "type": { "id": "t3", "name": "Comment" },
            "id": "c-1", "op": "insert", "after": { "properties": { "slug": "c-1" } } }),
        );
        let added = Learnt::counted(&[comment, edge], false);
        assert_eq!(added.about().as_deref(), Some("Comment c-1"));
        assert!(added.touches("lk-acl"));
        assert_eq!(Learnt::said(CommitCounts::First).about(), None);
    }
}
