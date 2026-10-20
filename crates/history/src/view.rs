//! The history tab (spec 009): a branch's commits newest first, a lane graph for merges, and
//! the selected commit's changes in a detail split.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use editor::{Editor, EditorEvent};
use gpui::{
    AnyElement, AnyView, App, Bounds, ClipboardItem, Context, Entity, EventEmitter, FocusHandle,
    Focusable, Hsla, KeyContext, PathBuilder, Pixels, Render, ScrollStrategy, SharedString,
    StyleRefinement, Subscription, Task, UniformListScrollHandle, WeakEntity, Window, canvas,
    point, px, uniform_list,
};
use graph_client::ReadError;
use graph_client::api::CommitOutput;
use graph_store::{GraphSession, HeadWatch, SessionEvent};
use studio_actions::graph::OpenCommit;
use studio_actions::history::{NextMatch, PreviousMatch};
use studio_ui::menu::ContextMenu;
use studio_ui::{change_counts, now_micros, relative_time, short_commit_id};
use studio_workspace::{Item, ItemEvent, SavedItem};
use ui::prelude::*;
use ui::{Severity, Tooltip, right_click_menu};

use crate::commit_view::{CommitCounts, CommitView};
use crate::lanes::{Graph, GraphCommit, assign_lanes};
use crate::merged::{branch_heads, move_head, unresolved_parents, with_merged_commits};

/// Above this many commits the list warns that it is read and laid out whole: History asks
/// for a branch's whole commit list, not a page of it.
pub const LONG_HISTORY: usize = 5_000;

/// How long scrolling must pause before the drawn rows' change counts are read, and stepping
/// through commits before the selected one's changes are.
const COUNTS_AFTER: Duration = Duration::from_millis(120);
/// How long typing in the search pauses before the commits are matched.
const SEARCH_DELAY: Duration = Duration::from_millis(80);
/// Other branches whose lists History reads for heads and merged-in commits.
const OTHER_BRANCHES: usize = 20;

const ROW_HEIGHT: f32 = studio_ui::ds::size::ROW;
/// The short commit id's column, and the time's, so ids and times line up down the list.
const COMMIT_ID_WIDTH: f32 = 84.;
const WHEN_WIDTH: f32 = 64.;
/// The change counts' column, so `+3 ~2 −1` line up down the list whatever tags follow.
const CHANGES_WIDTH: f32 = 112.;
const LANE_WIDTH: f32 = 14.;
const GRAPH_PADDING: f32 = studio_ui::ds::space::MD;

/// Changes read per commit for a row's counts.
const ROW_COUNT_PAGE: u32 = 500;

enum State {
    Loading,
    Ready,
    Failed(SharedString),
}

pub struct HistoryView {
    session: Entity<GraphSession>,
    branch: String,
    /// The bottom-dock panel: rows only, following the window's branch; a commit opens in its
    /// own tab.
    compact: bool,
    /// Finds commits by id prefix or actor.
    search: Entity<Editor>,
    matches: Vec<usize>,
    match_at: Option<usize>,
    /// The search text the matches were found for, lowercase: typing finds them once it pauses.
    match_query: String,
    _search_task: Option<Task<()>>,
    focus_handle: FocusHandle,
    state: State,
    commits: Vec<CommitOutput>,
    /// What the search finds each commit by, made with the list.
    commit_texts: Vec<CommitText>,
    graph: Graph,
    selected: Option<usize>,
    detail: Option<Entity<CommitView>>,
    /// The commit list, drawn cached on its own: the commit beside it scrolls and reads
    /// without the list drawing again.
    rows: Entity<HistoryRows>,
    counts: HashMap<String, (usize, usize, usize)>,
    /// Commits whose counts come from a first page only, and those asked for already.
    partial_counts: HashSet<String>,
    asked_counts: HashSet<String>,
    /// The count reads under way, by commit; and the drawn rows they were last asked for,
    /// with the pause before they are read.
    count_tasks: HashMap<String, Task<()>>,
    count_rows: Option<Range<usize>>,
    _count_wait: Option<Task<()>>,
    /// The rows the frame being drawn asked for last, taken once it is out ([`Self::drawn`]).
    drawn_rows: Option<Range<usize>>,
    drawn_due: bool,
    /// Branch names by the commit they point at.
    heads: HashMap<String, Vec<String>>,
    /// Commits in the branch's own list; the rest came in through merges.
    own_count: usize,
    scroll: UniformListScrollHandle,
    _task: Option<Task<()>>,
    _detail_subscription: Option<Subscription>,
    _search_subscription: Subscription,
    _session_subscription: Option<Subscription>,
    /// A tab's own branch asked after while it is open, so another writer's commits to it
    /// arrive; the panel's is the session's, always asked after.
    _head_watch: Option<HeadWatch>,
    /// The other branches whose lists the history read: a commit to one shows here too, as
    /// its head or a merge from it.
    others: Vec<String>,
    /// The parents and merged parents the history names without holding, from its last
    /// layout: a commit to another branch lays it out again only when that branch's list now
    /// holds one.
    unresolved: Arc<HashSet<String>>,
    /// Other branches' lists read again after a commit to them, for their heads, by branch.
    head_reads: HashMap<String, Task<()>>,
}

impl EventEmitter<ItemEvent> for HistoryView {}

impl Focusable for HistoryView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for HistoryView {
    /// The commit list is drawn cached on its own (`HistoryRows`).
    fn caches_its_parts(&self) -> bool {
        true
    }

    fn tab_title(&self, _cx: &App) -> SharedString {
        format!("History · {}", self.branch).into()
    }

    fn tab_icon(&self, _cx: &App) -> Option<IconName> {
        Some(IconName::HistoryRerun)
    }

    fn dedupe_key(&self, _cx: &App) -> Option<SharedString> {
        Some(history_key(&self.branch).into())
    }

    fn serialize(&self, _cx: &App) -> Option<SavedItem> {
        Some(SavedItem {
            kind: "history".into(),
            data: serde_json::json!({ "branch": self.branch }),
        })
    }
}

pub fn history_key(branch: &str) -> String {
    format!("history:{branch}")
}

impl HistoryView {
    pub fn new(
        session: Entity<GraphSession>,
        branch: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self::build(session.clone(), branch, false, window, cx);
        view._head_watch =
            Some(session.update(cx, |session, cx| session.watch_head(&view.branch, cx)));
        // Its own branch's commits, and the other branches whose heads and merges it shows: a
        // tab restored before the branch list was read draws them once it is.
        view._session_subscription =
            Some(cx.subscribe(&session, |this, _, event, cx| match event {
                SessionEvent::Committed { branch } if this.shows(branch) => {
                    this.committed(branch, cx)
                }
                SessionEvent::BranchesChanged => this.reload(cx),
                _ => {}
            }));
        view
    }

    /// The bottom-dock panel: the window's branch, rows only.
    pub fn panel(
        session: Entity<GraphSession>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let branch = session.read(cx).branch.clone();
        let mut view = Self::build(session.clone(), branch, true, window, cx);
        view._session_subscription =
            Some(
                cx.subscribe(&session, |this, session, event, cx| match event {
                    SessionEvent::BranchChanged => {
                        this.branch = session.read(cx).branch.clone();
                        this.forget_branch();
                        this.reload(cx);
                    }
                    SessionEvent::Committed { branch } if this.shows(branch) => {
                        this.committed(branch, cx)
                    }
                    // The list of other branches, whose heads and merges the history shows.
                    SessionEvent::BranchesChanged => this.reload(cx),
                    _ => {}
                }),
            );
        view
    }

    fn build(
        session: Entity<GraphSession>,
        branch: String,
        compact: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Find a commit by id or actor", window, cx);
            editor.set_text_style_refinement(studio_ui::ds::field_text(cx));
            editor
        });
        let search_subscription = cx.subscribe(&search, |this, _, event: &EditorEvent, cx| {
            if let EditorEvent::BufferEdited = event {
                // Once typing pauses: each search goes through every commit and selects the
                // first it finds.
                this._search_task = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(SEARCH_DELAY).await;
                    this.update(cx, |this, cx| this.find(cx)).ok();
                }));
            }
        });
        let mut view = Self {
            session,
            branch,
            compact,
            search,
            matches: Vec::new(),
            match_at: None,
            match_query: String::new(),
            _search_task: None,
            _search_subscription: search_subscription,
            _session_subscription: None,
            _head_watch: None,
            others: Vec::new(),
            unresolved: Arc::default(),
            head_reads: HashMap::new(),
            focus_handle: cx.focus_handle(),
            state: State::Loading,
            commits: Vec::new(),
            commit_texts: Vec::new(),
            graph: Graph::default(),
            selected: None,
            detail: None,
            rows: HistoryRows::new(cx),
            counts: HashMap::new(),
            partial_counts: HashSet::new(),
            asked_counts: HashSet::new(),
            count_tasks: HashMap::new(),
            count_rows: None,
            _count_wait: None,
            drawn_rows: None,
            drawn_due: false,
            heads: HashMap::new(),
            own_count: 0,
            scroll: UniformListScrollHandle::new(),
            _task: None,
            _detail_subscription: None,
        };
        view.reload(cx);
        view
    }

    /// Lets go of what was shown for the branch left, so its commits never show under the new
    /// one's name while that is read.
    fn forget_branch(&mut self) {
        self.commits.clear();
        self.commit_texts.clear();
        self.graph = Graph::default();
        self.selected = None;
        self.detail = None;
        self._detail_subscription = None;
        self.heads.clear();
        self.own_count = 0;
        self.matches.clear();
        self.match_at = None;
        self.count_rows = None;
    }

    /// Shows the branch's history again from the session's commit lists, which are read again
    /// only for branches with new commits.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.load(false, cx);
    }

    /// Refresh: reads every branch's commits again, the other branches' too, to see commits
    /// made elsewhere: another branch's head, and a merge from it, show from its list.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.load(true, cx);
    }

    /// Whether a commit to `branch` changes what the history shows: its own branch's, or one
    /// of the others it read.
    fn shows(&self, branch: &str) -> bool {
        self.branch == branch || self.others.iter().any(|other| other == branch)
    }

    /// A commit landed on `branch`. One to the history's own branch, or one while the history
    /// is read, lays it out again. One to another branch it reads moves that branch's head
    /// tag only: that branch's new commits are not in this history, unless its list now holds
    /// a parent the history named without holding, which lays it out again.
    fn committed(&mut self, branch: &str, cx: &mut Context<Self>) {
        if branch == self.branch || matches!(self.state, State::Loading) {
            self.reload(cx);
            return;
        }
        let read = self
            .session
            .update(cx, |session, cx| session.commits(branch, false, cx));
        let unresolved = self.unresolved.clone();
        let name = branch.to_owned();
        let task = cx.spawn(async move |this, cx| {
            // Gone through off the main thread: a list holds the branch's whole history.
            let (head, holds_unresolved) = match read.await {
                Ok(list) => {
                    cx.background_spawn(async move {
                        let head = list.first().map(|c| c.graph_commit_id.clone());
                        let holds = list.iter().any(|c| unresolved.contains(&c.graph_commit_id));
                        (head, holds)
                    })
                    .await
                }
                // As a layout leaves out a branch whose list could not be read.
                Err(_) => (None, false),
            };
            this.update(cx, |this, cx| {
                this.head_reads.remove(&name);
                if holds_unresolved {
                    this.reload(cx);
                } else {
                    move_head(&mut this.heads, &name, head.as_deref());
                    cx.notify();
                }
            })
            .ok();
        });
        self.head_reads.insert(branch.to_owned(), task);
    }

    fn load(&mut self, fresh: bool, cx: &mut Context<Self>) {
        self.state = State::Loading;
        let branch = self.branch.clone();
        // Other live branches: their heads, and the commits merged in from them. All the
        // lists are asked for at once, and come from the session's lists where it has them.
        let others: Vec<String> = self
            .session
            .read(cx)
            .branches
            .loaded()
            .map(|branches| {
                branches
                    .iter()
                    .filter(|name| **name != branch)
                    .take(OTHER_BRANCHES)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let own_read = self
            .session
            .update(cx, |session, cx| session.commits(&branch, fresh, cx));
        // A head read for a branch the history no longer reads would put its tag back.
        self.head_reads.retain(|name, _| others.contains(name));
        self.others = others.clone();
        let other_reads: Vec<_> = others
            .into_iter()
            .map(|name| {
                let read = self
                    .session
                    .update(cx, |session, cx| session.commits(&name, fresh, cx));
                (name, read)
            })
            .collect();
        self._task = Some(cx.spawn(async move |this, cx| {
            // The shared lists go to the background as they are; copying thousands of commits
            // on the main thread would hold up frames.
            let own = match own_read.await {
                Ok(list) => list,
                Err(error) => {
                    this.update(cx, |this, cx| {
                        this.state = State::Failed(error.to_string().into());
                        cx.notify();
                    })
                    .ok();
                    return;
                }
            };
            let mut others = Vec::new();
            for (name, read) in other_reads {
                if let Ok(list) = read.await {
                    others.push((name, list));
                }
            }
            // Laid out off the main thread; a long history is laid out whole.
            let own_count = own.len();
            let (commits, commit_texts, layout, heads, unresolved) = cx
                .background_executor()
                .spawn(async move {
                    let own: Vec<CommitOutput> = own.as_ref().clone();
                    let heads = branch_heads(
                        std::iter::once((branch.as_str(), own.as_slice())).chain(
                            others
                                .iter()
                                .map(|(name, commits)| (name.as_str(), commits.as_slice())),
                        ),
                    );
                    let lists: Vec<Vec<CommitOutput>> = others
                        .into_iter()
                        .map(|(_, commits)| commits.as_ref().clone())
                        .collect();
                    let commits = with_merged_commits(own, &lists);
                    let nodes: Vec<GraphCommit> = commits
                        .iter()
                        .map(|c| GraphCommit {
                            id: c.graph_commit_id.clone(),
                            parents: c
                                .parent_commit_id
                                .iter()
                                .chain(c.merged_parent_commit_id.iter())
                                .cloned()
                                .collect(),
                        })
                        .collect();
                    let layout = assign_lanes(&nodes);
                    let commit_texts: Vec<CommitText> =
                        commits.iter().map(CommitText::of).collect();
                    let unresolved = unresolved_parents(&commits);
                    (commits, commit_texts, layout, heads, unresolved)
                })
                .await;
            this.update(cx, |this, cx| {
                let previous = this
                    .selected
                    .and_then(|ix| this.commits.get(ix))
                    .map(|c| c.graph_commit_id.clone());
                this.commits = commits;
                this.commit_texts = commit_texts;
                this.graph = layout;
                this.heads = heads;
                this.unresolved = Arc::new(unresolved);
                this.own_count = own_count;
                this.state = State::Ready;
                // The matches and the drawn rows' counts were found in the list before.
                this.match_commits(cx);
                this.count_rows = None;
                let keep = previous
                    .and_then(|id| this.commits.iter().position(|c| c.graph_commit_id == id));
                match keep {
                    Some(ix) => this.selected = Some(ix),
                    None if !this.commits.is_empty() => this.select(0, cx),
                    None => {}
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// The commits the search text finds ([`CommitText::matches`]); the first is selected.
    fn find(&mut self, cx: &mut Context<Self>) {
        self.match_commits(cx);
        self.match_at = (!self.matches.is_empty()).then_some(0);
        if let Some(&ix) = self.matches.first() {
            self.step_to(ix, cx);
        }
        cx.notify();
    }

    /// Finds the search text's commits in the list as it is now, keeping the selection.
    fn match_commits(&mut self, cx: &App) {
        let query = self.search.read(cx).text(cx).trim().to_lowercase();
        self.matches = if query.is_empty() {
            Vec::new()
        } else {
            self.commit_texts
                .iter()
                .enumerate()
                .filter(|(_, text)| text.matches(&query))
                .map(|(ix, _)| ix)
                .collect()
        };
        self.match_query = query;
        self.match_at = self
            .match_at
            .filter(|_| !self.matches.is_empty())
            .map(|at| at.min(self.matches.len() - 1));
    }

    fn step_match(&mut self, forward: bool, cx: &mut Context<Self>) {
        // Within the pause after typing, the matches are for the text before: they are found
        // for the text the box holds, which selects the first.
        if self.search.read(cx).text(cx).trim().to_lowercase() != self.match_query {
            self._search_task = None;
            self.find(cx);
            return;
        }
        let count = self.matches.len();
        let Some(at) = self.match_at.filter(|_| count > 0) else {
            return;
        };
        let at = if forward {
            (at + 1) % count
        } else {
            (at + count - 1) % count
        };
        self.match_at = Some(at);
        self.step_to(self.matches[at], cx);
    }

    /// Selects a commit and, in the tab, shows its changes in the detail split.
    pub fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.show(ix, None, cx);
    }

    /// Selects a commit reached by a key: its changes are read once the steps pause, so a
    /// held arrow or a search being typed reads only where it stops.
    fn step_to(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.show(ix, Some(COUNTS_AFTER), cx);
    }

    fn show(&mut self, ix: usize, pause: Option<Duration>, cx: &mut Context<Self>) {
        let Some(commit) = self.commits.get(ix).cloned() else {
            return;
        };
        self.selected = Some(ix);
        self.scroll.scroll_to_item(ix, ScrollStrategy::Center);
        if self.compact {
            cx.notify();
            return;
        }
        let same = self
            .detail
            .as_ref()
            .is_some_and(|d| d.read(cx).commit_id() == commit.graph_commit_id);
        if !same {
            // The detail counts its commit's changes whole: the row does not read them too. A
            // commit left before its detail counted it is asked for again.
            if let Some(left) = self
                .detail
                .as_ref()
                .map(|d| d.read(cx).commit_id().to_owned())
                && !self.counts.contains_key(&left)
            {
                self.asked_counts.remove(&left);
                self.count_rows = None;
            }
            self.asked_counts.insert(commit.graph_commit_id.clone());
            let session = self.session.clone();
            let id = commit.graph_commit_id.clone();
            let detail = cx.new(|cx| match pause {
                Some(pause) => CommitView::after_pause(session, id, Some(commit), pause, cx),
                None => CommitView::new(session, id, Some(commit), cx),
            });
            self._detail_subscription =
                Some(cx.subscribe(&detail, |this, _, counts: &CommitCounts, cx| {
                    if counts.partial {
                        this.partial_counts.insert(counts.commit_id.clone());
                    } else {
                        this.partial_counts.remove(&counts.commit_id);
                    }
                    this.counts.insert(
                        counts.commit_id.clone(),
                        (counts.added, counts.changed, counts.removed),
                    );
                    cx.notify();
                }));
            self.detail = Some(detail);
        }
        cx.notify();
    }

    fn select_next(&mut self, _: &menu::SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        let next = self.selected.map_or(0, |s| s + 1);
        if next < self.commits.len() {
            self.step_to(next, cx);
        }
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(previous) = self.selected.and_then(|s| s.checked_sub(1)) {
            self.step_to(previous, cx);
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(commit) = self.selected.and_then(|ix| self.commits.get(ix)) {
            window.dispatch_action(
                Box::new(OpenCommit {
                    commit_id: commit.graph_commit_id.clone(),
                }),
                cx,
            );
        }
    }

    fn graph_width(&self) -> f32 {
        ((self.graph.lane_count.max(1) as f32) * LANE_WIDTH + GRAPH_PADDING * 2.).max(52.)
    }

    /// Notes the rows a frame draws. The list also lays one row out twice a frame to measure
    /// it, so the rows are taken once the frame is out, as the last range it asked for: else
    /// every redraw would restart the pause before the counts are read.
    fn drawn(&mut self, rows: Range<usize>, window: &mut Window, cx: &mut Context<Self>) {
        self.drawn_rows = Some(rows);
        if !std::mem::replace(&mut self.drawn_due, true) {
            cx.defer_in(window, |this, _, cx| {
                this.drawn_due = false;
                if let Some(rows) = this.drawn_rows.clone() {
                    this.want_row_counts(rows, cx);
                }
            });
        }
    }

    /// Asks for the drawn rows' change counts once scrolling has paused: a fast scroll reads
    /// nothing for the rows it passes, and reads still out for rows no longer drawn are dropped
    /// (and asked again if they come back).
    fn want_row_counts(&mut self, rows: Range<usize>, cx: &mut Context<Self>) {
        if self.count_rows.as_ref() == Some(&rows) {
            return;
        }
        self.count_rows = Some(rows.clone());
        self._count_wait = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(COUNTS_AFTER).await;
            this.update(cx, |this, cx| {
                let drawn: HashSet<&str> = this
                    .commits
                    .get(rows.start..rows.end.min(this.commits.len()))
                    .unwrap_or_default()
                    .iter()
                    .map(|commit| commit.graph_commit_id.as_str())
                    .collect();
                let gone: Vec<String> = this
                    .count_tasks
                    .keys()
                    .filter(|id| !drawn.contains(id.as_str()))
                    .cloned()
                    .collect();
                for id in gone {
                    this.count_tasks.remove(&id);
                    this.asked_counts.remove(&id);
                }
                this.load_row_counts(rows, cx);
            })
            .ok();
        }));
    }

    /// Reads change counts for drawn commits that have none yet: one page each, netted as the
    /// detail nets them. A commit with more changes than the page shows "N+". A commit's
    /// changes never change, so an answer is kept for the life of the view.
    fn load_row_counts(&mut self, range: Range<usize>, cx: &mut Context<Self>) {
        let ids: Vec<String> = self
            .commits
            .get(range.start..range.end.min(self.commits.len()))
            .unwrap_or_default()
            .iter()
            .map(|commit| commit.graph_commit_id.clone())
            .filter(|id| !self.counts.contains_key(id) && !self.asked_counts.contains(id))
            .collect();
        for id in ids {
            self.asked_counts.insert(id.clone());
            let key = id.clone();
            let read = self.session.update(cx, |session, cx| {
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
                            this.count_tasks.remove(&id);
                            if !lasting {
                                this.asked_counts.remove(&id);
                            }
                        })
                        .ok();
                        return;
                    }
                };
                let more = page.next_page_token.is_some();
                let folded = cx
                    .background_spawn(async move {
                        let mut fold = graph_store::ChangeFold::new();
                        fold.add_block(graph_client::api::ChangeBlockOutput {
                            cause: page.cause,
                            changes: page.changes,
                        });
                        fold.finish()
                            .counts()
                            .iter()
                            .fold((0, 0, 0), |(a, c, r), t| {
                                (a + t.added, c + t.changed, r + t.removed)
                            })
                    })
                    .await;
                this.update(cx, |this, cx| {
                    this.count_tasks.remove(&id);
                    // The detail's full count, if it arrived meanwhile, wins.
                    if !this.counts.contains_key(&id) {
                        this.counts.insert(id.clone(), folded);
                        if more {
                            this.partial_counts.insert(id);
                        }
                        cx.notify();
                    }
                })
                .ok();
            });
            self.count_tasks.insert(key, task);
        }
    }

    fn render_rows(
        &mut self,
        range: Range<usize>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let selected_bg = studio_ui::ds::surface::selected_document(cx);
        let hover_bg = studio_ui::ds::surface::hover(cx);
        // One accent for the branch's own lane; merged branches' lanes in a quiet grey, so the
        // graph reads as structure, not a palette.
        let accent = studio_ui::ds::surface::accent(cx);
        let quiet = Color::Muted
            .color(cx)
            .opacity(studio_ui::ds::opacity::DIMMED);
        let graph_width = self.graph_width();
        let now = now_micros();
        range
            .filter_map(|ix| {
                let commit = self.commits.get(ix)?;
                let row = self.graph.rows.get(ix)?.clone();
                let color = |index: usize| if index == 0 { accent } else { quiet };
                let lines = RowPaint {
                    commit: (row.lane, color(row.color)),
                    through: row
                        .through
                        .iter()
                        .map(|l| (l.lane, color(l.color)))
                        .collect(),
                    incoming: row
                        .incoming
                        .iter()
                        .map(|l| (l.lane, color(l.color)))
                        .collect(),
                    outgoing: row
                        .outgoing
                        .iter()
                        .map(|l| (l.lane, color(l.color)))
                        .collect(),
                    merge: commit.merged_parent_commit_id.is_some(),
                    // A merged parent History could not read (a deleted branch's commit).
                    stub: commit.merged_parent_commit_id.is_some()
                        && !row.outgoing.iter().any(|l| l.lane != row.lane),
                };
                let branch = commit
                    .graph_branch
                    .clone()
                    .unwrap_or_else(|| "main".to_owned());
                let id = commit.graph_commit_id.clone();
                let counts = self.counts.get(&id).copied();
                let partial = self.partial_counts.contains(&id);
                let heads = self.heads.get(&id).cloned().unwrap_or_default();
                let selected = self.selected == Some(ix);
                let actor = commit.actor_id.clone();
                let open_id = id.clone();
                let copy_id = id.clone();
                let merge = commit.merged_parent_commit_id.is_some();
                // A row reads: lane, id, where it heads, what it changed, then who and when.
                let row_element = h_flex()
                    .id(("commit-row", ix))
                    .h(px(ROW_HEIGHT))
                    .mx(px(studio_ui::ds::space::SM))
                    .pr(px(studio_ui::ds::space::MD))
                    .gap(px(studio_ui::ds::space::LG))
                    .rounded(px(studio_ui::ds::radii(cx).sm))
                    .when(selected, |d| d.bg(selected_bg))
                    .when(!selected, |d| d.hover(|d| d.bg(hover_bg)))
                    .cursor_pointer()
                    .child(
                        div().w(px(graph_width)).h_full().flex_none().child(
                            canvas(
                                |_, _, _| {},
                                move |bounds, _, window, _| lines.paint(bounds, window),
                            )
                            .size_full(),
                        ),
                    )
                    .child(div().w(px(COMMIT_ID_WIDTH)).flex_none().child(
                        studio_ui::ds::text::code(short_commit_id(&id), cx).color(if selected {
                            Color::Default
                        } else {
                            Color::Muted
                        }),
                    ))
                    .child(
                        div()
                            .w(px(CHANGES_WIDTH))
                            .flex_none()
                            .overflow_hidden()
                            .children(counts.map(|(added, changed, removed)| {
                                if added + changed + removed == 0 {
                                    studio_ui::ds::text::meta("no changes").into_any_element()
                                } else {
                                    h_flex()
                                        .gap(px(studio_ui::ds::space::XS))
                                        .child(change_counts(added, changed, removed))
                                        .when(partial, |d| {
                                            d.child(studio_ui::ds::text::meta("+ more"))
                                        })
                                        .into_any_element()
                                }
                            })),
                    )
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(px(studio_ui::ds::space::SM))
                            .overflow_hidden()
                            .children(
                                heads
                                    .iter()
                                    .map(|name| studio_ui::ds::tag(name.clone(), cx)),
                            )
                            .when(merge, |d| {
                                d.child(
                                    div()
                                        .id(("merge", ix))
                                        .tooltip(Tooltip::text(format!("Merged into {branch}")))
                                        .child(studio_ui::ds::icon::meta(IconName::PullRequest)),
                                )
                            }),
                    )
                    .child(
                        div()
                            .max_w(px(studio_ui::ds::TAG_MAX_WIDTH))
                            .flex_none()
                            .child(
                                studio_ui::ds::text::meta(actor.unwrap_or_else(|| "—".into()))
                                    .single_line()
                                    .truncate(),
                            ),
                    )
                    .child(
                        h_flex().w(px(WHEN_WIDTH)).flex_none().justify_end().child(
                            studio_ui::ds::text::meta(relative_time(commit.created_at, now))
                                .single_line(),
                        ),
                    )
                    .on_click(
                        cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                            this.select(ix, cx);
                            // The panel has no detail split: a click opens the commit's tab.
                            if this.compact || event.click_count() == 2 {
                                window.dispatch_action(
                                    Box::new(OpenCommit {
                                        commit_id: open_id.clone(),
                                    }),
                                    cx,
                                );
                            }
                        }),
                    );
                Some(
                    right_click_menu(("commit-menu", ix))
                        .trigger(move |_, _, _| row_element)
                        .menu(move |window, cx| {
                            let copy_id = copy_id.clone();
                            ContextMenu::build(window, cx, move |menu, _, _| {
                                let open_id = copy_id.clone();
                                menu.entry("Copy commit id", None, move |_, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        copy_id.clone(),
                                    ))
                                })
                                .entry(
                                    "Open in a tab",
                                    None,
                                    move |window, cx| {
                                        window.dispatch_action(
                                            Box::new(OpenCommit {
                                                commit_id: open_id.clone(),
                                            }),
                                            cx,
                                        )
                                    },
                                )
                            })
                        })
                        .into_any_element(),
                )
            })
            .collect()
    }

    fn render_list(&self, cx: &mut Context<Self>) -> AnyElement {
        // No column header: an id, tags, counts, an actor and a time say what they are.
        v_flex()
            .size_full()
            .pt(px(studio_ui::ds::space::SM))
            .child(
                uniform_list(
                    "history-rows",
                    self.commits.len(),
                    cx.processor(|this, range: Range<usize>, window, cx| {
                        this.drawn(range.clone(), window, cx);
                        this.render_rows(range, window, cx)
                    }),
                )
                .track_scroll(&self.scroll)
                .flex_1()
                .min_h_0(),
            )
            .into_any_element()
    }
}

/// What the search finds a commit by, lowercase, made once with the list rather than at each
/// key: its id as stored and as a row shows it (its last eight characters), and its actor.
struct CommitText {
    id: String,
    short: String,
    actor: Option<String>,
}

impl CommitText {
    fn of(commit: &CommitOutput) -> Self {
        Self {
            id: commit.graph_commit_id.to_lowercase(),
            short: short_commit_id(&commit.graph_commit_id).to_lowercase(),
            actor: commit.actor_id.as_ref().map(|actor| actor.to_lowercase()),
        }
    }

    /// Whether the search text `query` (lowercase) begins the commit's id, either way, or is
    /// in its actor.
    fn matches(&self, query: &str) -> bool {
        self.id.starts_with(query)
            || self.short.starts_with(query)
            || self
                .actor
                .as_deref()
                .is_some_and(|actor| actor.contains(query))
    }
}

/// One row's lines, resolved to colors, painted into the row's graph cell.
struct RowPaint {
    commit: (usize, Hsla),
    through: Vec<(usize, Hsla)>,
    incoming: Vec<(usize, Hsla)>,
    outgoing: Vec<(usize, Hsla)>,
    merge: bool,
    stub: bool,
}

impl RowPaint {
    fn paint(&self, bounds: Bounds<Pixels>, window: &mut Window) {
        let x = |lane: usize| {
            bounds.origin.x + px(GRAPH_PADDING + LANE_WIDTH * lane as f32 + LANE_WIDTH / 2.)
        };
        let top = bounds.origin.y;
        let bottom = bounds.origin.y + bounds.size.height;
        let mid = top + bounds.size.height / 2.;
        let (lane, color) = self.commit;
        let stroke = |from: gpui::Point<Pixels>,
                      to: gpui::Point<Pixels>,
                      control: Option<gpui::Point<Pixels>>,
                      color: Hsla,
                      window: &mut Window| {
            let mut builder = PathBuilder::stroke(px(1.5));
            builder.move_to(from);
            match control {
                Some(control) => builder.curve_to(to, control),
                None => builder.line_to(to),
            }
            if let Ok(path) = builder.build() {
                window.paint_path(path, color);
            }
        };
        for &(l, c) in &self.through {
            stroke(point(x(l), top), point(x(l), bottom), None, c, window);
        }
        for &(l, c) in &self.incoming {
            let control = (l != lane).then(|| point(x(l), mid));
            stroke(point(x(l), top), point(x(lane), mid), control, c, window);
        }
        for &(l, c) in &self.outgoing {
            let control = (l != lane).then(|| point(x(l), mid));
            stroke(point(x(lane), mid), point(x(l), bottom), control, c, window);
        }
        if self.stub {
            let end = point(
                x(lane) + px(LANE_WIDTH * 0.8),
                top + bounds.size.height * 0.9,
            );
            stroke(
                point(x(lane), mid),
                end,
                Some(point(end.x, mid)),
                color.opacity(studio_ui::ds::opacity::DIMMED),
                window,
            );
        }
        let radius = if self.merge { px(4.5) } else { px(3.5) };
        let center = point(x(lane), mid);
        let mut circle = PathBuilder::fill();
        circle.move_to(point(center.x + radius, center.y));
        circle.arc_to(
            point(radius, radius),
            px(0.),
            false,
            true,
            point(center.x - radius, center.y),
        );
        circle.arc_to(
            point(radius, radius),
            px(0.),
            false,
            true,
            point(center.x + radius, center.y),
        );
        circle.close();
        if let Ok(path) = circle.build() {
            window.paint_path(path, color);
        }
    }
}

/// The commit list as a view of its own, drawn again when the history changes (it observes
/// it), not when the commit shown beside it scrolls or reads (`docs/drawing.md`).
struct HistoryRows {
    history: WeakEntity<HistoryView>,
    _history_changed: Subscription,
    /// How often the rows were drawn, for the test that the commit beside them leaves them.
    #[cfg(test)]
    renders: usize,
}

impl HistoryRows {
    /// Made with the view, not on its first draw.
    fn new(cx: &mut Context<HistoryView>) -> Entity<Self> {
        let history = cx.entity();
        cx.new(|cx| HistoryRows {
            history: history.downgrade(),
            _history_changed: cx.observe(&history, |_, _, cx| cx.notify()),
            #[cfg(test)]
            renders: 0,
        })
    }
}

impl Render for HistoryRows {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(test)]
        {
            self.renders += 1;
        }
        let list = self
            .history
            .update(cx, |history, cx| history.render_list(cx))
            .unwrap_or_else(|_| gpui::Empty.into_any_element());
        v_flex().size_full().child(list)
    }
}

impl HistoryView {
    fn cached_rows(&self) -> AnyElement {
        AnyView::from(self.rows.clone())
            .cached(StyleRefinement::default().size_full())
            .into_any_element()
    }
}

impl Render for HistoryView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.commits.len();
        let searched = !self.match_query.is_empty();
        let counter = match (searched, self.match_at) {
            (false, _) => None,
            (true, Some(at)) => Some(format!("{}/{}", at + 1, self.matches.len())),
            (true, None) => Some("No match".to_owned()),
        };
        let search = studio_ui::ds::search_field(self.search.clone(), cx)
            .w(px(if self.compact { 200. } else { 240. }))
            .children(counter.map(studio_ui::ds::text::meta));
        let summary = match self.state {
            State::Ready if count > self.own_count => {
                format!("{count} commits · {} merged in", count - self.own_count)
            }
            State::Ready => format!("{count} commit{}", if count == 1 { "" } else { "s" }),
            State::Loading => "Reading…".into(),
            State::Failed(_) => String::new(),
        };
        let header = studio_ui::ds::toolbar(cx)
            .when(!self.compact, |header| {
                header
                    .child(studio_ui::ds::icon::lead(IconName::HistoryRerun))
                    .child(studio_ui::ds::text::title("History"))
            })
            // One quiet fact after the title, as the table's row count: the tab already names
            // the branch.
            .child(studio_ui::ds::text::muted(summary))
            .child(div().flex_1())
            .child(search)
            .child(
                studio_ui::ds::toolbar_icon_button("refresh-history", IconName::RotateCw)
                    .icon_color(Color::Muted)
                    .tooltip(Tooltip::text("Read the history again"))
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
            );
        let warning = (count > LONG_HISTORY).then(|| {
            div().px(px(studio_ui::ds::space::XL)).py(px(studio_ui::ds::space::XS)).child(
                studio_ui::ds::banner(Severity::Warning)
                    .child(
                        studio_ui::ds::text::body(format!(
                            "{count} commits. History reads the whole list at once, which takes a while for a history this long."
                        )),
                    ),
            )
        });
        let body = match &self.state {
            State::Failed(error) => {
                studio_ui::ds::error_state("History could not be read", error.clone())
                    .into_any_element()
            }
            State::Loading if self.commits.is_empty() => {
                studio_ui::ds::loading_state("Reading commits…").into_any_element()
            }
            _ if self.compact => self.cached_rows(),
            _ => {
                let list = self.cached_rows();
                let detail = match &self.detail {
                    Some(detail) => detail.clone().into_any_element(),
                    None => studio_ui::ds::empty_state(IconName::HistoryRerun, "Select a commit")
                        .into_any_element(),
                };
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(div().flex_1().min_w_0().h_full().child(list))
                    .child(
                        div()
                            .w(relative(0.46))
                            .flex_none()
                            .h_full()
                            .border_l_1()
                            .border_color(studio_ui::ds::surface::rule(cx))
                            .child(detail),
                    )
                    .into_any_element()
            }
        };
        // Typing in the search box must not move the selection.
        let mut context = KeyContext::new_with_defaults();
        context.add("History");
        if self.search.focus_handle(cx).is_focused(window) {
            context.add("searching");
        }
        v_flex()
            .key_context(context)
            .track_focus(&self.focus_handle)
            .size_full()
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(|this, _: &NextMatch, _, cx| this.step_match(true, cx)))
            .on_action(cx.listener(|this, _: &PreviousMatch, _, cx| this.step_match(false, cx)))
            .child(header)
            .children(warning)
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use gpui::TestAppContext;
    use graph_store::{Connection, ServerEntry, ServerSource};
    use http_client::{AsyncBody, FakeHttpClient, Response};
    use serde_json::json;

    use super::*;

    /// A History tab opened before the branch list was read reads the other branches, whose
    /// heads and merges it shows, once the list is in.
    #[gpui::test]
    async fn a_tab_opened_before_the_branch_list_reads_the_other_branches(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let lists = Arc::new(Mutex::new(Vec::<String>::new()));
        let asked = lists.clone();
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            let query = request.uri().query().unwrap_or_default().to_owned();
            let asked = asked.clone();
            async move {
                let body = if path.ends_with("/branches") {
                    json!({ "branches": ["main", "fix"] })
                } else if path.ends_with("/commits") {
                    asked.lock().unwrap().push(query);
                    json!({ "commits": [] })
                } else {
                    json!({})
                };
                Ok(Response::builder()
                    .status(200)
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
            cx.new(|cx| GraphSession::new(connection, "knowledge".into(), "main".into(), cx));
        let (_view, cx) = cx.add_window_view(move |window, cx| {
            HistoryView::new(session, "main".into(), window, cx)
        });
        cx.run_until_parked();
        let lists = lists.lock().unwrap();
        assert!(
            lists.iter().any(|query| query.contains("branch=fix")),
            "{lists:?}"
        );
    }

    /// A commit to another branch moves that branch's tag without laying the history out
    /// again; one whose list now holds a merged parent the history could not find lays it out
    /// again, with that commit.
    #[gpui::test]
    async fn a_commit_to_another_branch_moves_its_tag_alone(cx: &mut TestAppContext) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        // `main` merged `fix` at f1, and x1 from a list not read; `fix` then commits f2, then
        // x1 shows in its list.
        let fix_version = Arc::new(AtomicUsize::new(0));
        let served = fix_version.clone();
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            let query = request.uri().query().unwrap_or_default().to_owned();
            let version = served.load(Ordering::SeqCst);
            async move {
                let commit = |id: &str, at: i64, parent: Option<&str>, merged: Option<&str>| {
                    json!({
                        "graph_commit_id": id, "graph_manifest_version": at, "created_at": at,
                        "parent_commit_id": parent, "merged_parent_commit_id": merged,
                    })
                };
                let body = if path.ends_with("/branches") {
                    json!({ "branches": ["main", "fix"] })
                } else if path.ends_with("/commits") && query.contains("branch=fix") {
                    let mut commits = vec![
                        commit("f1", 2, Some("m1"), None),
                        commit("m1", 1, None, None),
                    ];
                    if version >= 1 {
                        commits.insert(0, commit("f2", 4, Some("f1"), None));
                    }
                    if version >= 2 {
                        commits.insert(0, commit("x1", 5, Some("f2"), None));
                    }
                    json!({ "commits": commits })
                } else if path.ends_with("/commits") {
                    json!({ "commits": [
                        commit("m3", 6, Some("m2"), Some("x1")),
                        commit("m2", 3, Some("m1"), Some("f1")),
                        commit("m1", 1, None, None),
                    ] })
                } else {
                    json!({})
                };
                Ok(Response::builder()
                    .status(200)
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
            cx.new(|cx| GraphSession::new(connection, "knowledge".into(), "main".into(), cx));
        let (view, cx) = cx.add_window_view({
            let session = session.clone();
            move |window, cx| HistoryView::new(session, "main".into(), window, cx)
        });
        cx.run_until_parked();
        let tag = |view: &HistoryView, commit: &str| view.heads.get(commit).cloned();
        let laid_out = view.read_with(cx, |view, _| {
            assert_eq!(tag(view, "f1"), Some(vec!["fix".to_owned()]));
            view.commits.as_ptr()
        });
        // Another writer's commit to `fix`, as the session hears of one: its list is read again.
        let commit_to_fix = |version: usize, cx: &mut gpui::VisualTestContext| {
            fix_version.store(version, Ordering::SeqCst);
            let read = session.update(cx, |session, cx| {
                let read = session.commits("fix", true, cx);
                cx.emit(SessionEvent::Committed {
                    branch: "fix".into(),
                });
                read
            });
            cx.run_until_parked();
            drop(read);
        };
        commit_to_fix(1, cx);
        view.read_with(cx, |view, _| {
            assert_eq!(view.commits.as_ptr(), laid_out, "laid out again");
            assert_eq!(tag(view, "f2"), Some(vec!["fix".to_owned()]));
            assert_eq!(tag(view, "f1"), None);
        });
        commit_to_fix(2, cx);
        view.read_with(cx, |view, _| {
            assert_ne!(view.commits.as_ptr(), laid_out, "x1 joins the history");
            assert!(view.commits.iter().any(|c| c.graph_commit_id == "x1"));
            assert_eq!(tag(view, "x1"), Some(vec!["fix".to_owned()]));
        });
    }

    /// A session on a graph whose `main` holds one commit, `c1`, which changed nothing.
    fn one_commit_session(cx: &mut TestAppContext) -> Entity<GraphSession> {
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            async move {
                let body = if path.ends_with("/branches") {
                    json!({ "branches": ["main"] })
                } else if path.ends_with("/changes") {
                    json!({
                        "cause": {"graph_commit_id": "c1", "authored_branch": "main", "authored_at": 1},
                        "changes": [],
                    })
                } else if path.ends_with("/commits") {
                    json!({ "commits": [{
                        "graph_commit_id": "c1", "graph_manifest_version": 1, "created_at": 1,
                    }] })
                } else {
                    json!({})
                };
                Ok(Response::builder()
                    .status(200)
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
        cx.new(|cx| GraphSession::new(connection, "knowledge".into(), "main".into(), cx))
    }

    /// The commit shown beside the list draws on its own: a change there (a scroll, a page of
    /// changes read) leaves the list as it was drawn; a change of the history draws it again.
    #[gpui::test]
    async fn the_commit_beside_the_list_leaves_the_list_drawn(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let session = one_commit_session(cx);
        let (view, cx) = cx.add_window_view(move |window, cx| {
            HistoryView::new(session, "main".into(), window, cx)
        });
        cx.run_until_parked();
        view.update(cx, |view, cx| view.select(0, cx));
        cx.run_until_parked();
        let renders = |cx: &mut gpui::VisualTestContext| {
            view.read_with(cx, |view, cx| view.rows.read(cx).renders)
        };
        let detail = view.read_with(cx, |view, _| view.detail.clone().expect("a commit shown"));
        let before = renders(cx);
        detail.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        assert_eq!(renders(cx), before, "the commit drew the list again");
        view.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        assert_eq!(renders(cx), before + 1);
    }

    /// Typing searches once it pauses, not at each key, and the counter speaks of the text the
    /// matches were found for.
    #[gpui::test]
    async fn a_search_matches_the_commits_once_typing_pauses(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let session = one_commit_session(cx);
        let (view, cx) = cx.add_window_view(move |window, cx| {
            HistoryView::new(session, "main".into(), window, cx)
        });
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            view.search
                .update(cx, |editor, cx| editor.set_text("C1", window, cx))
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(view.matches.is_empty(), "matched before typing paused");
            assert!(view.match_query.is_empty());
        });
        cx.executor().advance_clock(SEARCH_DELAY);
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.matches, [0]);
            assert_eq!(view.match_at, Some(0));
            assert_eq!(view.match_query, "c1");
        });
        // A step taken before typing pauses steps among the matches for the text typed.
        view.update_in(cx, |view, window, cx| {
            view.search
                .update(cx, |editor, cx| editor.set_text("nothing", window, cx));
            view.step_match(true, cx);
            assert!(view.matches.is_empty());
            assert_eq!(view.match_at, None);
            assert_eq!(view.match_query, "nothing");
        });
    }

    #[test]
    fn search_finds_commits_by_the_start_of_their_id_or_their_actor() {
        let commit = CommitOutput {
            graph_commit_id: "hb1.01M4R84Z6GANJ0B8MJ4S3Y6CKK.2.01M4R865CRM266GRK5EP12CTBE".into(),
            graph_manifest_version: 3,
            created_at: 0,
            graph_branch: None,
            parent_commit_id: None,
            merged_parent_commit_id: None,
            actor_id: Some("act-Dev".into()),
            kind: Default::default(),
        };
        let text = CommitText::of(&commit);
        assert!(text.matches("hb1.01m4r"));
        assert!(text.matches("dev"));
        assert!(text.matches("ep12ctbe"), "the id as a row shows it");
        assert!(text.matches("ep12"));
        assert!(!text.matches("12ctbe"), "an id matches by its start only");
        assert!(!text.matches("alice"));
    }
}
