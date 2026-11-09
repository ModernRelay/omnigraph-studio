//! The left dock's "Commits" panel (spec 026): the newest commits of every branch, newest first,
//! read again as commits land, each opening the commit's tab.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use gpui::{
    Context, Entity, ListAlignment, ListState, Render, Subscription, Task, WeakEntity, Window, list,
};
use graph_client::api::CommitOutput;
use graph_store::{GraphSession, HeadWatch, Loadable, SessionEvent};
use studio_ui::ds::Tipped;
use studio_ui::{now_micros, relative_time, short_commit_id};
use studio_workspace::{Workspace, WorkspaceEvent};
use ui::prelude::*;

pub const COMMITS_PANEL: &str = "commits";

/// Commits read of each branch: its newest page.
const PAGE: u32 = 50;
/// Branches watched and read, in the session's order (main first).
const BRANCHES: usize = 20;
/// Rows shown at most.
const ROWS: usize = 200;
/// How often a shown panel draws its times again.
const AGE_TICK: Duration = Duration::from_secs(60);

pub struct CommitsPanel {
    session: Option<Entity<GraphSession>>,
    /// Each branch read, by name: its newest commits, or why they could not be read.
    pages: HashMap<String, Loadable<Vec<CommitOutput>>>,
    /// Branches to read when the panel is next drawn: a hidden panel reads nothing.
    owed: HashSet<String>,
    /// The session asks after each branch's head while its watch is held.
    watches: Vec<(String, HeadWatch)>,
    reads: HashMap<String, Task<()>>,
    list_state: ListState,
    rows: Vec<Row>,
    /// The commit last clicked, by id.
    selected: Option<String>,
    /// The drawn commits' change counts, as History reads them (spec 009).
    counts: crate::counts::RowCounts,
    /// The rows the frame being drawn laid out, asked for once it is out.
    drawn: Vec<usize>,
    _session_subscription: Option<Subscription>,
    _tick: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

/// One row, as what it shows, so the list is told only of the rows that changed.
#[derive(Clone)]
enum Row {
    Reading,
    Empty,
    Failed { branch: String, error: String },
    Commit(Box<CommitOutput>),
}

/// A commit never changes, so two of its rows are the same row.
impl PartialEq for Row {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Row::Reading, Row::Reading) | (Row::Empty, Row::Empty) => true,
            (
                Row::Failed { branch, error },
                Row::Failed {
                    branch: other_branch,
                    error: other_error,
                },
            ) => branch == other_branch && error == other_error,
            (Row::Commit(a), Row::Commit(b)) => a.graph_commit_id == b.graph_commit_id,
            _ => false,
        }
    }
}

/// The rows for the branches read: their commits once each, under the branch that made it,
/// newest first; then the branches that could not be read.
fn rows_of(pages: &HashMap<String, Loadable<Vec<CommitOutput>>>) -> Vec<Row> {
    let mut seen = HashSet::new();
    let mut commits: Vec<&CommitOutput> = pages
        .values()
        .filter_map(|page| match page {
            Loadable::Loaded(commits) => Some(commits.iter()),
            _ => None,
        })
        .flatten()
        .filter(|commit| seen.insert(commit.graph_commit_id.as_str()))
        .collect();
    commits.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| a.graph_commit_id.cmp(&b.graph_commit_id))
    });
    let mut rows: Vec<Row> = commits
        .into_iter()
        .take(ROWS)
        .map(|commit| Row::Commit(Box::new(commit.clone())))
        .collect();
    let mut failed: Vec<Row> = pages
        .iter()
        .filter_map(|(branch, page)| match page {
            Loadable::Failed(error) => Some(Row::Failed {
                branch: branch.clone(),
                error: error.clone(),
            }),
            _ => None,
        })
        .collect();
    failed.sort_by(|a, b| match (a, b) {
        (Row::Failed { branch: a, .. }, Row::Failed { branch: b, .. }) => a.cmp(b),
        _ => std::cmp::Ordering::Equal,
    });
    if rows.is_empty() && failed.is_empty() {
        let reading = pages.is_empty()
            || pages
                .values()
                .any(|page| matches!(page, Loadable::NotLoaded | Loadable::Loading));
        rows.push(if reading { Row::Reading } else { Row::Empty });
    }
    rows.extend(failed);
    rows
}

/// The branch that made `commit`, as the panel names it.
fn branch_of(commit: &CommitOutput) -> &str {
    commit.graph_branch.as_deref().unwrap_or("main")
}

impl CommitsPanel {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        session: Option<Entity<GraphSession>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut subscriptions = Vec::new();
        if let Some(workspace) = workspace.upgrade() {
            subscriptions.push(cx.subscribe_in(
                &workspace,
                window,
                |this, workspace, event: &WorkspaceEvent, _, cx| {
                    if let WorkspaceEvent::SessionChanged = event {
                        let session = workspace.read(cx).session().cloned();
                        this.watch_session(session, cx);
                    }
                },
            ));
        }
        let mut this = Self {
            session: None,
            pages: HashMap::new(),
            owed: HashSet::new(),
            watches: Vec::new(),
            reads: HashMap::new(),
            list_state: ListState::new(
                0,
                ListAlignment::Top,
                px(studio_ui::ds::size::SIDEBAR_ROW * 8.),
            ),
            rows: Vec::new(),
            selected: None,
            counts: Default::default(),
            drawn: Vec::new(),
            _session_subscription: None,
            _tick: None,
            _subscriptions: subscriptions,
        };
        this.watch_session(session, cx);
        this
    }

    /// Follows another graph's session: what was read of the last one goes.
    fn watch_session(&mut self, session: Option<Entity<GraphSession>>, cx: &mut Context<Self>) {
        if self.session.as_ref().map(Entity::entity_id) == session.as_ref().map(Entity::entity_id) {
            return;
        }
        self.pages.clear();
        self.owed.clear();
        self.watches.clear();
        self.reads.clear();
        self.selected = None;
        self.counts = Default::default();
        self._session_subscription = session.as_ref().map(|session| {
            cx.subscribe(session, |this, _, event: &SessionEvent, cx| match event {
                SessionEvent::Committed { branch } if this.pages.contains_key(branch) => {
                    this.owed.insert(branch.clone());
                    cx.notify();
                }
                SessionEvent::BranchesChanged => {
                    this.sync_branches(cx);
                    cx.notify();
                }
                _ => {}
            })
        });
        self.session = session;
        self.sync_branches(cx);
        cx.notify();
    }

    /// Watches and reads the branches the session lists, up to [`BRANCHES`]; a branch gone is
    /// dropped with its commits.
    fn sync_branches(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else {
            return;
        };
        let branches: Vec<String> = match &session.read(cx).branches {
            Loadable::Loaded(branches) => branches.iter().take(BRANCHES).cloned().collect(),
            _ => vec![session.read(cx).branch.clone()],
        };
        self.pages.retain(|branch, _| branches.contains(branch));
        self.reads.retain(|branch, _| branches.contains(branch));
        self.owed.retain(|branch| branches.contains(branch));
        self.watches.retain(|(branch, _)| branches.contains(branch));
        for branch in branches {
            if self.pages.contains_key(&branch) {
                continue;
            }
            self.pages.insert(branch.clone(), Loadable::NotLoaded);
            self.owed.insert(branch.clone());
            let watch = session.update(cx, |session, cx| session.watch_head(&branch, cx));
            self.watches.push((branch, watch));
        }
    }

    /// Reads `branch`'s newest page. One read per branch is out at a time: a newer one replaces
    /// it, so an older answer never lands after it.
    fn read(&mut self, branch: String, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else {
            return;
        };
        if let Some(page @ Loadable::NotLoaded) = self.pages.get_mut(&branch) {
            *page = Loadable::Loading;
        }
        let task = session.update(cx, |session, cx| {
            session.commit_page(&branch, PAGE, None, cx)
        });
        let key = branch.clone();
        self.reads.insert(
            key,
            cx.spawn(async move |this, cx| {
                let result = task.await;
                this.update(cx, |this, cx| {
                    this.reads.remove(&branch);
                    // A branch dropped meanwhile stays dropped.
                    if let Some(page) = this.pages.get_mut(&branch) {
                        *page = match result {
                            Ok(output) => Loadable::Loaded(output.commits),
                            Err(error) => Loadable::Failed(error.to_string()),
                        };
                        cx.notify();
                    }
                })
                .ok();
            }),
        );
    }

    /// A click previews the commit's tab, with the commit as listed; a double click keeps it
    /// (spec 020).
    fn open(&mut self, commit: &CommitOutput, preview: bool, window: &mut Window, cx: &mut App) {
        let Some(session) = self.session.clone() else {
            return;
        };
        let Some(workspace) = studio_workspace::workspace_for(window) else {
            return;
        };
        let (id, listed) = (commit.graph_commit_id.clone(), commit.clone());
        window.defer(cx, move |window, cx| {
            workspace.update(cx, |workspace, cx| {
                crate::commit_view::open_in(
                    workspace,
                    session,
                    id,
                    Some(listed),
                    preview,
                    window,
                    cx,
                )
            });
        });
    }

    /// After a frame: asks for the change counts of the commits it drew.
    fn want_counts(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else {
            return;
        };
        let drawn: Vec<&CommitOutput> = std::mem::take(&mut self.drawn)
            .into_iter()
            .filter_map(|ix| match self.rows.get(ix) {
                Some(Row::Commit(commit)) => Some(commit.as_ref()),
                _ => None,
            })
            .collect();
        let mut counts = std::mem::take(&mut self.counts);
        counts.want(&session, drawn, |this: &mut Self| &mut this.counts, cx);
        self.counts = counts;
    }

    fn render_row(&mut self, ix: usize, now: i64, cx: &mut Context<Self>) -> AnyElement {
        use studio_ui::ds;
        if self.drawn.is_empty() {
            let panel = cx.entity().downgrade();
            cx.defer(move |cx| {
                panel.update(cx, |this, cx| this.want_counts(cx)).ok();
            });
        }
        self.drawn.push(ix);
        let Some(row) = self.rows.get(ix).cloned() else {
            return div().into_any_element();
        };
        match row {
            Row::Reading => ds::sidebar_note("Reading commits…").into_any_element(),
            Row::Empty => ds::sidebar_placeholder(
                "commits-empty",
                IconName::GitCommit,
                "No commits",
                "Commits made on any branch show here as they land",
            )
            .into_any_element(),
            Row::Failed { branch, error } => ds::sidebar_placeholder(
                SharedString::from(format!("commits-failed-{branch}")),
                IconName::Warning,
                format!("{branch} could not be read"),
                error,
            )
            .into_any_element(),
            Row::Commit(commit) => {
                let id = short_commit_id(&commit.graph_commit_id);
                let actor = commit.actor_id.clone();
                let branch = branch_of(&commit).to_owned();
                let when = relative_time(commit.created_at, now);
                let label = match &actor {
                    Some(actor) => format!("{id} {actor}"),
                    None => id.clone(),
                };
                let tip = format!(
                    "{} on {branch}, by {}, {when}",
                    commit.graph_commit_id,
                    actor.as_deref().unwrap_or("no one named")
                );
                let selected = self.selected.as_deref() == Some(commit.graph_commit_id.as_str());
                let panel = cx.entity().downgrade();
                // What it changed, as History counts it, before where and when.
                let counts = self.counts.of(&commit.graph_commit_id).map(
                    |((added, changed, removed), partial)| {
                        h_flex()
                            .gap(px(ds::space::XS))
                            .child(studio_ui::change_counts(added, changed, removed))
                            .when(partial, |d| d.child(ds::text::meta("+")))
                    },
                );
                ds::SidebarRow::new(("commit", ix), label)
                    .icon(IconName::GitCommit)
                    .end(
                        h_flex()
                            .gap(px(ds::space::SM))
                            .children(counts)
                            .child(ds::sidebar_count(format!("{branch} · {when}"))),
                    )
                    .selected(selected)
                    .tip(ds::tooltip(tip))
                    .on_click(move |event, window, cx| {
                        let preview = event.click_count() < 2;
                        panel
                            .update(cx, |panel, cx| {
                                panel.selected = Some(commit.graph_commit_id.clone());
                                panel.open(&commit, preview, window, cx);
                                cx.notify();
                            })
                            .ok();
                    })
                    .into_any_element()
            }
        }
    }
}

impl Render for CommitsPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // What is owed is read once the panel is drawn, after the frame.
        if !self.owed.is_empty() {
            let owed: Vec<String> = self.owed.drain().collect();
            cx.defer_in(window, move |this, _, cx| {
                for branch in owed {
                    this.read(branch, cx);
                }
            });
        }
        if self._tick.is_none() && self.rows.iter().any(|row| matches!(row, Row::Commit(_))) {
            self._tick = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(AGE_TICK).await;
                this.update(cx, |this, cx| {
                    this._tick = None;
                    cx.notify();
                })
                .ok();
            }));
        }
        let rows = rows_of(&self.pages);
        studio_ui::rows::sync(&self.list_state, &mut self.rows, rows);
        let now = now_micros();
        v_flex().key_context("CommitsPanel").size_full().child(
            list(
                self.list_state.clone(),
                cx.processor(move |this, ix, _, cx| {
                    div()
                        .w_full()
                        .pb(px(studio_ui::ds::space::WITHIN))
                        .child(this.render_row(ix, now, cx))
                        .into_any_element()
                }),
            )
            .size_full(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use graph_client::api::CommitKind;

    fn commit(id: &str, branch: Option<&str>, at: i64) -> CommitOutput {
        CommitOutput {
            graph_commit_id: id.into(),
            graph_manifest_version: 1,
            created_at: at,
            graph_branch: branch.map(Into::into),
            parent_commit_id: None,
            merged_parent_commit_id: None,
            actor_id: Some("ada".into()),
            kind: CommitKind::Published,
        }
    }

    fn ids(rows: &[Row]) -> Vec<String> {
        rows.iter()
            .map(|row| match row {
                Row::Commit(commit) => commit.graph_commit_id.clone(),
                Row::Failed { branch, .. } => format!("failed {branch}"),
                Row::Reading => "reading".into(),
                Row::Empty => "empty".into(),
            })
            .collect()
    }

    /// Every branch's newest commits, newest first, each once under the branch that made it;
    /// a branch that could not be read says so after them.
    #[test]
    fn rows_are_every_branchs_newest_commits_once_each() {
        let mut pages = HashMap::new();
        assert_eq!(ids(&rows_of(&pages)), ["reading"]);
        pages.insert("main".to_owned(), Loadable::Loading);
        assert_eq!(ids(&rows_of(&pages)), ["reading"]);
        pages.insert(
            "main".to_owned(),
            Loadable::Loaded(vec![commit("m2", None, 20), commit("m1", None, 10)]),
        );
        // A branch's chain holds main's commits from before it began.
        pages.insert(
            "feature".to_owned(),
            Loadable::Loaded(vec![
                commit("f1", Some("feature"), 30),
                commit("m1", None, 10),
            ]),
        );
        pages.insert("gone".to_owned(), Loadable::Failed("refused".into()));
        let rows = rows_of(&pages);
        assert_eq!(ids(&rows), ["f1", "m2", "m1", "failed gone"]);
        let Row::Commit(first) = &rows[0] else {
            panic!("a commit");
        };
        assert_eq!(branch_of(first), "feature");
        let Row::Commit(second) = &rows[1] else {
            panic!("a commit");
        };
        assert_eq!(branch_of(second), "main");

        let mut empty = HashMap::new();
        empty.insert("main".to_owned(), Loadable::Loaded(Vec::new()));
        assert_eq!(ids(&rows_of(&empty)), ["empty"]);
    }
}
