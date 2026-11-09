//! The change counts beside a listed commit (spec 009), as History and the Commits panel
//! (spec 026) show them: one page of the commit's changes each, read for the rows drawn once
//! scrolling pauses, and kept, since a commit's changes never change.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use gpui::{AppContext as _, Context, Entity, Task};
use graph_client::ReadError;
use graph_client::api::{CommitOutput, EntityChangeOutput};
use graph_store::{GraphSession, NetChange};

/// Changes read per commit for a row's counts: each comes with its before and after images, and a
/// drawn row asks for its own. A commit with more shows its counts as partial.
pub(crate) const ROW_COUNT_PAGE: u32 = 100;

/// The pause in scrolling before the drawn rows' counts are read.
pub(crate) const COUNTS_AFTER: Duration = Duration::from_millis(120);

/// Added, changed and removed entities.
pub(crate) type Counts = (usize, usize, usize);

/// The entities a page of one commit's changes adds, changes and removes. A commit's changes name
/// each entity once, so they are counted where the session's memo holds them, with nothing
/// copied to fold them.
pub(crate) fn tally(changes: &[EntityChangeOutput]) -> Counts {
    changes.iter().fold(
        (0, 0, 0),
        |(added, changed, removed), change| match NetChange::of(
            change.before.as_ref(),
            change.after.as_ref(),
        ) {
            Some(NetChange::Added) => (added + 1, changed, removed),
            Some(NetChange::Changed) => (added, changed + 1, removed),
            Some(NetChange::Removed) => (added, changed, removed + 1),
            None => (added, changed, removed),
        },
    )
}

/// The counts read for a view's commits, and the reads under way.
#[derive(Default)]
pub(crate) struct RowCounts {
    pub(crate) counts: HashMap<String, Counts>,
    /// Commits whose counts come from a first page only, and those asked for already.
    partial: HashSet<String>,
    asked: HashSet<String>,
    /// The reads under way, by commit; and the commits drawn they were last asked for, with
    /// the pause before they are read.
    tasks: HashMap<String, Task<()>>,
    drawn: Option<Vec<String>>,
    _wait: Option<Task<()>>,
}

impl RowCounts {
    /// A commit's counts, and whether they are its first page's only.
    pub(crate) fn of(&self, id: &str) -> Option<(Counts, bool)> {
        Some((*self.counts.get(id)?, self.partial.contains(id)))
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
            .filter(|id| !self.counts.contains_key(id) && !self.asked.contains(id))
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
                        // A commit that cannot be compared (a schema change: 409) or is past
                        // cleanup (410) has no count, now or later: it stays asked, so a row
                        // scrolled back to does not ask again. Another failure may pass.
                        let lasting = matches!(
                            &error,
                            ReadError::Refused(refusal) if matches!(refusal.status, 409 | 410)
                        );
                        this.update(cx, |this, _| {
                            let counts = of(this);
                            counts.tasks.remove(&id);
                            if !lasting {
                                counts.asked.remove(&id);
                            }
                        })
                        .ok();
                        return;
                    }
                };
                let more = page.next_page_token.is_some();
                let folded = cx
                    .background_spawn(async move { tally(&page.changes) })
                    .await;
                this.update(cx, |this, cx| {
                    let counts = of(this);
                    counts.tasks.remove(&id);
                    counts.counts.insert(id.clone(), folded);
                    if more {
                        counts.partial.insert(id);
                    }
                    cx.notify();
                })
                .ok();
            });
            self.tasks.insert(key, task);
        }
    }
}
