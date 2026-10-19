//! The history tab (spec 009): a branch's commits newest first, a lane graph for merges, and
//! the selected commit's changes in a detail split.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::time::Duration;

use editor::{Editor, EditorEvent};
use gpui::{
    AnyElement, App, Bounds, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable,
    Hsla, KeyContext, PathBuilder, Pixels, Render, ScrollStrategy, SharedString, Subscription,
    Task, UniformListScrollHandle, Window, canvas, point, px, uniform_list,
};
use graph_client::api::CommitOutput;
use graph_store::{GraphSession, SessionEvent};
use studio_actions::graph::OpenCommit;
use studio_actions::history::{NextMatch, PreviousMatch};
use studio_ui::menu::ContextMenu;
use studio_ui::{change_counts, now_micros, relative_time, short_commit_id};
use studio_workspace::{Item, ItemEvent, SavedItem};
use ui::prelude::*;
use ui::{Banner, Severity, Tooltip, right_click_menu};

use crate::commit_view::{CommitCounts, CommitView};
use crate::lanes::{Graph, GraphCommit, assign_lanes};
use crate::merged::{branch_heads, with_merged_commits};

/// Above this many commits the list warns that it loads in full (E10).
pub const LONG_HISTORY: usize = 5_000;

/// Other branches whose lists History reads for heads and merged-in commits.
/// How long scrolling must pause before the drawn rows' change counts are read.
const COUNTS_AFTER: Duration = Duration::from_millis(120);
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
    focus_handle: FocusHandle,
    state: State,
    commits: Vec<CommitOutput>,
    graph: Graph,
    selected: Option<usize>,
    detail: Option<Entity<CommitView>>,
    counts: HashMap<String, (usize, usize, usize)>,
    /// Commits whose counts come from a first page only, and those asked for already.
    partial_counts: HashSet<String>,
    asked_counts: HashSet<String>,
    /// The count reads under way, by commit; and the drawn rows they were last asked for,
    /// with the pause before they are read.
    count_tasks: HashMap<String, Task<()>>,
    count_rows: Option<Range<usize>>,
    _count_wait: Option<Task<()>>,
    /// Branch names by the commit they point at.
    heads: HashMap<String, Vec<String>>,
    /// Commits in the branch's own list; the rest came in through merges.
    own_count: usize,
    scroll: UniformListScrollHandle,
    _task: Option<Task<()>>,
    _detail_subscription: Option<Subscription>,
    _search_subscription: Subscription,
    _session_subscription: Option<Subscription>,
}

impl EventEmitter<ItemEvent> for HistoryView {}

impl Focusable for HistoryView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for HistoryView {
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
        Self::build(session, branch, false, window, cx)
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
                    SessionEvent::Committed { branch } if *branch == this.branch => this.reload(cx),
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
                this.find(cx);
            }
        });
        let mut view = Self {
            session,
            branch,
            compact,
            search,
            matches: Vec::new(),
            match_at: None,
            _search_subscription: search_subscription,
            _session_subscription: None,
            focus_handle: cx.focus_handle(),
            state: State::Loading,
            commits: Vec::new(),
            graph: Graph::default(),
            selected: None,
            detail: None,
            counts: HashMap::new(),
            partial_counts: HashSet::new(),
            asked_counts: HashSet::new(),
            count_tasks: HashMap::new(),
            count_rows: None,
            _count_wait: None,
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

    /// Refresh: reads the branch's own commits again, to see commits made elsewhere.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.load(true, cx);
    }

    fn load(&mut self, fresh: bool, cx: &mut Context<Self>) {
        self.state = State::Loading;
        let branch = self.branch.clone();
        // Other live branches: their heads, and the commits merged in from them (E9). All the
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
        let other_reads: Vec<_> = others
            .into_iter()
            .map(|name| {
                let read = self
                    .session
                    .update(cx, |session, cx| session.commits(&name, false, cx));
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
            // Lay out off the main thread; long histories load in full (E10).
            let own_count = own.len();
            let (commits, layout, heads) = cx
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
                    (commits, layout, heads)
                })
                .await;
            this.update(cx, |this, cx| {
                let previous = this
                    .selected
                    .and_then(|ix| this.commits.get(ix))
                    .map(|c| c.graph_commit_id.clone());
                this.commits = commits;
                this.graph = layout;
                this.heads = heads;
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

    /// The commits the search text finds ([`commit_matches`]); the first is selected.
    fn find(&mut self, cx: &mut Context<Self>) {
        self.match_commits(cx);
        self.match_at = (!self.matches.is_empty()).then_some(0);
        if let Some(&ix) = self.matches.first() {
            self.select(ix, cx);
        }
        cx.notify();
    }

    /// Finds the search text's commits in the list as it is now, keeping the selection.
    fn match_commits(&mut self, cx: &App) {
        let query = self.search.read(cx).text(cx).trim().to_lowercase();
        self.matches = if query.is_empty() {
            Vec::new()
        } else {
            self.commits
                .iter()
                .enumerate()
                .filter(|(_, commit)| commit_matches(commit, &query))
                .map(|(ix, _)| ix)
                .collect()
        };
        self.match_at = self
            .match_at
            .filter(|_| !self.matches.is_empty())
            .map(|at| at.min(self.matches.len() - 1));
    }

    fn step_match(&mut self, forward: bool, cx: &mut Context<Self>) {
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
        self.select(self.matches[at], cx);
    }

    /// Selects a commit and, in the tab, shows its changes in the detail split.
    pub fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
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
            let session = self.session.clone();
            let id = commit.graph_commit_id.clone();
            let detail = cx.new(|cx| CommitView::new(session, id, Some(commit), cx));
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
            self.select(next, cx);
        }
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(previous) = self.selected.and_then(|s| s.checked_sub(1)) {
            self.select(previous, cx);
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
                // A parentless commit or one past cleanup cannot be compared: no count.
                let Ok(page) = read.await else {
                    return;
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
        let quiet = Color::Muted.color(cx).opacity(0.55);
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
                                        .child(
                                            Icon::new(IconName::PullRequest)
                                                .size(IconSize::XSmall)
                                                .color(Color::Muted),
                                        ),
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
                        this.want_row_counts(range.clone(), cx);
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

/// Whether a commit's id starts with, or its actor contains, `query` (lowercase).
/// Whether the search text begins the commit's id, either as stored or as a row shows it (its
/// last eight characters), or is in its actor.
fn commit_matches(commit: &CommitOutput, query: &str) -> bool {
    commit.graph_commit_id.to_lowercase().starts_with(query)
        || short_commit_id(&commit.graph_commit_id)
            .to_lowercase()
            .starts_with(query)
        || commit
            .actor_id
            .as_ref()
            .is_some_and(|actor| actor.to_lowercase().contains(query))
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
                color.opacity(0.5),
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

impl Render for HistoryView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.commits.len();
        let searched = !self.search.read(cx).text(cx).trim().is_empty();
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
                    .child(
                        Icon::new(IconName::HistoryRerun)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(studio_ui::ds::text::title("History"))
            })
            .child(studio_ui::ds::meta_row([
                studio_ui::ds::meta(IconName::GitBranch, self.branch.clone()).into_any_element(),
                studio_ui::ds::text::meta(summary).into_any_element(),
            ]))
            .child(div().flex_1())
            .child(search)
            .child(
                studio_ui::ds::toolbar_icon_button("refresh-history", IconName::RotateCw)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted)
                    .tooltip(Tooltip::text("Read the history again"))
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
            );
        let warning = (count > LONG_HISTORY).then(|| {
            div().px(px(studio_ui::ds::space::XL)).py(px(studio_ui::ds::space::XS)).child(
                Banner::new()
                    .severity(Severity::Warning)
                    .child(
                        Label::new(format!(
                            "{count} commits. The list loads in full until the server pages history (E10)."
                        ))
                        .size(LabelSize::Small),
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
            _ if self.compact => self.render_list(cx),
            _ => {
                let list = self.render_list(cx);
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
    use super::*;

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
        assert!(commit_matches(&commit, "hb1.01m4r"));
        assert!(commit_matches(&commit, "dev"));
        assert!(
            commit_matches(&commit, "ep12ctbe"),
            "the id as a row shows it"
        );
        assert!(commit_matches(&commit, "ep12"));
        assert!(
            !commit_matches(&commit, "12ctbe"),
            "an id matches by its start only"
        );
        assert!(!commit_matches(&commit, "alice"));
    }
}
