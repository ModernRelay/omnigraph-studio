//! The history tab (spec 009): a branch's commits newest first, a lane graph for merges, and
//! the selected commit's changes in a detail split.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};

use editor::{Editor, EditorEvent};
use gpui::{
    AnyElement, AnyView, App, AsyncApp, Bounds, ClipboardItem, Context, Entity, EventEmitter,
    FocusHandle, Focusable, Hsla, KeyContext, PathBuilder, Pixels, Render, ScrollStrategy,
    SharedString, StyleRefinement, Subscription, Task, UniformListScrollHandle, WeakEntity, Window,
    canvas, point, px, uniform_list,
};
use graph_client::ReadError;
use graph_client::api::{CommitListOutput, CommitOutput};
use graph_store::{GraphSession, HeadWatch, SessionEvent};
use studio_actions::history::{NewBranchHere, NextMatch, PreviousMatch};
use studio_actions::workspace::Refresh;
use studio_ui::menu::ContextMenu;
use studio_ui::{change_counts, group_digits, now_micros, relative_time, short_commit_id};
use studio_workspace::{Item, ItemEvent, SavedItem};
use ui::prelude::*;
use ui::{Tooltip, right_click_menu};

use crate::commit_view::{CommitCounts, CommitView, branch_start, new_branch_at, open_listed};
use crate::lanes::{Graph, GraphCommit, assign_lanes};
use crate::merged::{Absent, Drawn, HistoryRead, branch_heads};

/// Commits read per page of the branch's history. A row is `ds::size::ROW` tall, so one page
/// fills several screens of the tab and the first shows after one read; a page costs the server
/// only the commits it returns, and each page read lays the list read so far out again off the
/// main thread, which stays quick at this size. The next page is read once the rows drawn come
/// within a screen of the list's end.
const PAGE: u32 = 200;
/// Merged-in commits read by id in one walk, at most, and how many at once. A walk follows
/// each page read, and another starts while the rows drawn name a merged-in commit not read yet
/// ([`HistoryView::want_merged`]). A merged branch's chain is read one commit at a time, since
/// each names only its parents, so one longer than this goes on in a later walk rather than
/// holding up the reads meanwhile.
const MERGED_READS: usize = PAGE as usize;
const MERGED_AT_ONCE: usize = 8;
/// How long a merged-in commit whose read failed (a timeout, a 5xx, a 429) waits before a row
/// naming it, drawn, asks for it again. Long enough that a server shedding load is not asked
/// again at once, and that a failure lasting minutes costs one read per commit every few
/// seconds, and only while its row is drawn; short enough that a passing failure heals while
/// the person looks.
const MERGED_RETRY_AFTER: Duration = Duration::from_secs(5);

/// How long scrolling must pause before the drawn rows' change counts are read, and stepping
/// through commits before the selected one's changes are.
const COUNTS_AFTER: Duration = Duration::from_millis(120);
/// How long typing in the search pauses before the commits are matched.
const SEARCH_DELAY: Duration = Duration::from_millis(80);
/// Other branches whose heads History reads for their tags, and whose listings it reads for
/// merged-in commits when the server refuses this token commits by id.
const OTHER_BRANCHES: usize = 20;
/// Commits read of each other branch's listing for a token the server refuses commits by id:
/// one page each, the server's most (`limit` is 1 to 1,000). A merged-in commit none of those
/// pages holds draws as a stub that says so, rather than the listings being read whole.
const LISTED_PAGE: u32 = 1_000;

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

/// The read of the listing's first page.
enum State {
    /// Being read. Commits read before stay shown meanwhile.
    Loading,
    Ready,
    /// It could not be read and no commit is shown: why takes the list's place.
    Failed(SharedString),
    /// Read again (a commit landing, a Refresh), it could not be read while commits were shown.
    /// They stay as read, and their pages and the commits merged into them go on being read;
    /// the toolbar says why and offers Try again ([`HistoryView::retry`]).
    Stale(SharedString),
}

/// The read of the listing's next page.
enum More {
    Idle,
    Reading,
    /// Why it failed: the end of the list says so and offers to try again, and scrolling reads
    /// nothing more until then. With `restart`, the server refused the page's continuation,
    /// which it would refuse again: Try again reads the listing from its first page.
    Failed {
        error: SharedString,
        restart: bool,
    },
}

/// How the listing goes on after a page.
struct Listing {
    /// The head the listing resolved, which each later page answers for.
    head: Option<String>,
    next: Option<String>,
    complete: bool,
}

impl Listing {
    fn of(page: &CommitListOutput) -> Self {
        // A page with no commits ends the listing: a token after nothing would only ask for
        // nothing again.
        let next = page
            .next_page_token
            .clone()
            .filter(|_| !page.commits.is_empty());
        Self {
            head: page.head_commit_id.clone(),
            complete: next.is_none(),
            next,
        }
    }
}

/// Whether a read that failed with `error` would fail so again: the server refused it with a
/// 4xx other than a timeout (408) or too many requests (429). A read that did not complete, or
/// met a busy or failing server, may pass when sent again.
fn refused_again(error: &ReadError) -> bool {
    matches!(
        error,
        ReadError::Refused(refusal)
            if (400..500).contains(&refusal.status) && !matches!(refusal.status, 408 | 429)
    )
}

/// The other branches' pages read for a token the server refuses commits by id
/// ([`HistoryView::read_listed`]). A branch whose page could not be read is kept out of
/// `commits`, so a merged-in commit none of the pages read holds is not taken for one no branch
/// holds ([`Absent::Unlisted`]).
#[derive(Clone, Default)]
struct Listed {
    /// The commits of the pages read, by id.
    commits: Arc<HashMap<String, CommitOutput>>,
    /// The branches whose page could not be read in passing (a timeout, a 5xx, a 429), and
    /// when the last read of them failed: they alone are read again once [`MERGED_RETRY_AFTER`]
    /// has passed, while a drawn row names a merged-in commit their pages may hold.
    failed: Vec<String>,
    failed_at: Option<Instant>,
    /// The branches whose page the server refused ([`refused_again`]: a branch the credential
    /// may not read, one deleted since the branch list was read). Read again at the next load
    /// or Refresh, not before, as the refusal would stand.
    refused: Vec<String>,
}

impl Listed {
    /// Every branch's page was read.
    fn complete(&self) -> bool {
        self.failed.is_empty() && self.refused.is_empty()
    }

    /// Whether the branches whose page failed in passing are read again at `now`.
    fn due(&self, now: Instant) -> bool {
        !self.failed.is_empty()
            && self
                .failed_at
                .is_none_or(|at| now.saturating_duration_since(at) >= MERGED_RETRY_AFTER)
    }

    /// Why a merged-in commit the pages read do not hold is not drawn while some page could not
    /// be read: it may be there.
    fn why_unlisted(&self) -> String {
        let names: Vec<&str> = self
            .failed
            .iter()
            .chain(&self.refused)
            .map(String::as_str)
            .collect();
        let lists = match names.as_slice() {
            [one] => format!("Branch {one}'s list"),
            more => format!("The lists of branches {}", more.join(", ")),
        };
        let again = if self.failed.is_empty() {
            "Refresh asks for it again."
        } else {
            "It is asked for again while this row shows."
        };
        format!(
            "{lists} could not be read, and the merged commit is not among the newest {} commits \
             of the other branches read. {again}",
            group_digits(LISTED_PAGE as usize)
        )
    }
}

/// A layout of what was read, made off the main thread: the commits drawn, what the search
/// finds them by, their lanes, and the merged-in commits to read next.
struct Arranged {
    read: Arc<HistoryRead>,
    commits: Vec<CommitOutput>,
    commit_texts: Vec<CommitText>,
    graph: Graph,
    wanted: Vec<String>,
}

/// A batch of merged-in commits read: laid out again, or (none sorting among the rows read)
/// only kept, with the ids answered.
enum Answered {
    Drawn(Arranged),
    Unseen(Arc<HistoryRead>, Vec<String>),
}

fn arrange(mut read: HistoryRead) -> Arranged {
    let Drawn { commits, wanted } = read.draw();
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
    let graph = assign_lanes(&nodes);
    let commit_texts = commits.iter().map(CommitText::of).collect();
    Arranged {
        read: Arc::new(read),
        commits,
        commit_texts,
        graph,
        wanted,
    }
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
    /// The first page's read.
    state: State,
    /// What was read of the branch's history: its own pages and the commits merged in.
    read: Arc<HistoryRead>,
    /// Where the listing goes on: the token after the last page read, and the head the
    /// listing's first page resolved, which every later page answers for, so the pages stay
    /// one history while the branch moves on.
    next_page: Option<String>,
    listing_head: Option<String>,
    more: More,
    /// The merged-in commits the commits drawn name and History has not read, top rows' first,
    /// and when each whose read failed since the last page read failed: it is asked for again
    /// after the next page read, or by a drawn row naming it once [`MERGED_RETRY_AFTER`] has
    /// passed, which the rows are drawn again for.
    wanted: Vec<String>,
    merged_failed: HashMap<String, Instant>,
    _merged_retry: Option<Task<()>>,
    /// Whether merged-in commits are read by id. The server checks that route against a read
    /// grant on every branch, so it refuses (403) a token whose grant names branches; merged-in
    /// commits then come from the other branches' listings, which it checks branch by branch,
    /// a page of [`LISTED_PAGE`] each, read once into `listed`, until the next load or Refresh;
    /// a page that could not be read in passing is read again alone ([`Listed::failed`]).
    by_id: bool,
    listed: Option<Listed>,
    /// The commits drawn, newest first: the branch's own and those merged in.
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
    scroll: UniformListScrollHandle,
    /// The read of the history under way, until it ends: a page, then the merged-in commits it
    /// names, or a walk of those alone.
    _task: Option<Task<()>>,
    _detail_subscription: Option<Subscription>,
    _search_subscription: Subscription,
    _session_subscription: Option<Subscription>,
    /// A tab's own branch asked after while it is open, so another writer's commits to it
    /// arrive; the panel's is the session's, always asked after.
    _head_watch: Option<HeadWatch>,
    /// The other branches whose heads the history shows as tags: a commit to one moves its
    /// tag.
    others: Vec<String>,
    /// Their heads as last read, by branch.
    other_heads: HashMap<String, String>,
    /// The head read last asked for each, by number, and the reads under way: only the one
    /// last asked for sets the tag, so a read sent before a commit, landing after one sent
    /// since, does not put the tag back.
    head_asked: HashMap<String, u64>,
    head_reads: HashMap<String, Task<()>>,
    next_head_read: u64,
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
        // Its own branch's commits, and the other branches whose heads it shows: a tab restored
        // before the branch list was read draws them once it is.
        view._session_subscription =
            Some(cx.subscribe(&session, |this, _, event, cx| match event {
                SessionEvent::Committed { branch } if this.shows(branch) => {
                    this.committed(branch, cx)
                }
                SessionEvent::BranchesChanged => this.branches_changed(cx),
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
                    // The list of other branches, whose heads the history shows.
                    SessionEvent::BranchesChanged => this.branches_changed(cx),
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
            other_heads: HashMap::new(),
            head_asked: HashMap::new(),
            head_reads: HashMap::new(),
            next_head_read: 0,
            focus_handle: cx.focus_handle(),
            state: State::Loading,
            read: Arc::default(),
            next_page: None,
            listing_head: None,
            more: More::Idle,
            wanted: Vec::new(),
            merged_failed: HashMap::new(),
            _merged_retry: None,
            by_id: true,
            listed: None,
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
            scroll: UniformListScrollHandle::new(),
            _task: None,
            _detail_subscription: None,
        };
        view.reload(cx);
        view
    }

    /// Lets go of what was shown and read for the branch left, so its commits never show under
    /// the new one's name while that is read; the branch left is one of the others now.
    fn forget_branch(&mut self) {
        self.read = Arc::default();
        self.next_page = None;
        self.listing_head = None;
        self.more = More::Idle;
        self.wanted.clear();
        self.merged_failed.clear();
        self._merged_retry = None;
        self.by_id = true;
        self.listed = None;
        self.commits.clear();
        self.commit_texts.clear();
        self.graph = Graph::default();
        self.selected = None;
        self.detail = None;
        self._detail_subscription = None;
        self.heads.clear();
        self.matches.clear();
        self.match_at = None;
        self.count_rows = None;
        self.others.clear();
        self.other_heads.clear();
        self.head_asked.clear();
        self.head_reads.clear();
    }

    /// Shows the branch's history again from its first page, and reads the heads of other
    /// branches not read yet.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.load(false, cx);
        self.load_heads(false, cx);
    }

    /// Refresh: reads the history again from its first page, the merged-in commits the server
    /// did not have, and every other branch's head again now. The session keeps a head until
    /// Studio sees a commit land on its branch, and another writer's commit to a branch nobody
    /// watches is seen only so; the head read again is the session's, so the branches panel
    /// and a later load show it too.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        Arc::make_mut(&mut self.read).forget_missing();
        self.load(false, cx);
        self.load_heads(true, cx);
    }

    /// Whether a commit to `branch` changes what the history shows: its own branch's, or one
    /// of the others whose heads it shows.
    fn shows(&self, branch: &str) -> bool {
        self.branch == branch || self.others.iter().any(|other| other == branch)
    }

    /// A commit landed on `branch`. One to the history's own branch reads its first page again,
    /// on top of the pages read ([`HistoryRead::first_page`]). One to another branch moves that
    /// branch's tag alone: its new commits are not this history's, and a merge of them would
    /// land on this branch.
    fn committed(&mut self, branch: &str, cx: &mut Context<Self>) {
        if branch == self.branch {
            self.load(true, cx);
        } else {
            self.read_head(branch.to_owned(), false, cx);
        }
    }

    /// The branch list changed: the tags follow the branches made and gone. A history that
    /// could not be read is read again, as its branch may have been made since; one whose first
    /// page could not be read again is read again as Try again reads it.
    fn branches_changed(&mut self, cx: &mut Context<Self>) {
        match self.state {
            State::Failed(_) => self.load(false, cx),
            State::Stale(_) => self.retry(cx),
            State::Loading | State::Ready => {}
        }
        self.load_heads(false, cx);
    }

    /// Try again, after the first page could not be read again while commits were shown: reads
    /// it again from the branch's head, and the pages read stay below it when it reaches the
    /// head they were read from, as for a commit landing.
    fn retry(&mut self, cx: &mut Context<Self>) {
        self.load(true, cx);
    }

    /// Whether the commits read are shown and go on being read: the first page was read, or
    /// a read of it again failed while they were shown.
    fn shown(&self) -> bool {
        matches!(self.state, State::Ready | State::Stale(_))
    }

    /// Reads the branch's first page and lays it out, then the merged-in commits it names. With
    /// `landed`, the pages read stay below it when it reaches the head they were read from
    /// ([`HistoryRead::first_page`]), and go on continuing their listing. Until it is read,
    /// what was read stays as it is: a read that fails changes none of it.
    fn load(&mut self, landed: bool, cx: &mut Context<Self>) {
        self.state = State::Loading;
        // A next page or a walk of merged-in commits under way ends here (`_task` is
        // replaced); one that failed stays failed, its row saying so.
        if matches!(self.more, More::Reading) {
            self.more = More::Idle;
        }
        let page = self.session.update(cx, |session, cx| {
            session.commit_page(&self.branch, PAGE, None, cx)
        });
        self._task = Some(cx.spawn(async move |this, cx| {
            Self::first_page(&this, page, landed, cx).await;
            this.update(cx, |this, _| this._task = None).ok();
        }));
        cx.notify();
    }

    async fn first_page(
        this: &WeakEntity<Self>,
        page: Task<Result<CommitListOutput, ReadError>>,
        landed: bool,
        cx: &mut AsyncApp,
    ) {
        let page = match page.await {
            Ok(page) => page,
            Err(error) => {
                this.update(cx, |this, cx| {
                    let error = error.to_string().into();
                    // Commits shown stay as read (constitution V): only a history showing none
                    // gives the list's place to why.
                    this.state = if this.commits.is_empty() {
                        State::Failed(error)
                    } else {
                        State::Stale(error)
                    };
                    cx.notify();
                })
                .ok();
                return;
            }
        };
        let Ok(held) = this.update(cx, |this, _| this.read.clone()) else {
            return;
        };
        let listing = Listing::of(&page);
        let complete = listing.complete;
        // What was read goes to the background as it is, and is copied there with the page:
        // copying thousands of commits on the main thread would hold up frames.
        let (arranged, kept) = cx
            .background_spawn(async move {
                let mut read = HistoryRead::clone(&held);
                // The other branches' pages are read again after this one, so what a page that
                // could not be read left unanswered is asked for again.
                read.forget_unlisted();
                let kept = read.first_page(page.commits, complete, landed);
                (arrange(read), kept)
            })
            .await;
        let laid_out = this.update(cx, |this, cx| {
            if !kept {
                this.next_page = listing.next;
                this.listing_head = listing.head;
            }
            this.state = State::Ready;
            this.more = More::Idle;
            // The merged-in commits this page names are read afresh: by id first, and those
            // whose read failed without waiting.
            this.merged_failed.clear();
            this.by_id = true;
            this.listed = None;
            this.take(arranged, cx);
        });
        if laid_out.is_ok() {
            Self::read_merged(this, cx).await;
        }
    }

    /// Reads the listing's next page, which continues the listing the first page resolved even
    /// after the branch moved on: one page at a time, none once the listing ends, and none after
    /// one failed until it is tried again. It takes the place of a walk of merged-in commits
    /// under way, which the walk after the page goes on with.
    fn load_next(&mut self, cx: &mut Context<Self>) {
        if !self.shown() || !matches!(self.more, More::Idle) {
            return;
        }
        let Some(token) = self.next_page.clone() else {
            return;
        };
        self.more = More::Reading;
        self.merged_failed.clear();
        let asked_head = self.listing_head.clone();
        let page = self.session.update(cx, |session, cx| {
            session.commit_page(&self.branch, PAGE, Some(token), cx)
        });
        self._task = Some(cx.spawn(async move |this, cx| {
            Self::next_page(&this, page, asked_head, cx).await;
            this.update(cx, |this, _| this._task = None).ok();
        }));
        cx.notify();
    }

    async fn next_page(
        this: &WeakEntity<Self>,
        page: Task<Result<CommitListOutput, ReadError>>,
        asked_head: Option<String>,
        cx: &mut AsyncApp,
    ) {
        let page = match page.await {
            Ok(page) => page,
            Err(error) => {
                // A continuation refused (its token rejected, say) is refused again when sent
                // again; one that did not complete, or met a busy server, is sent again.
                let restart = refused_again(&error);
                this.update(cx, |this, cx| {
                    this.more = More::Failed {
                        error: error.to_string().into(),
                        restart,
                    };
                    cx.notify();
                })
                .ok();
                return;
            }
        };
        // A page answering for another head is of another listing than the pages read, and is
        // not added to them. Reading the first page again here could go on for as long as the
        // server answers so; Try again reads it once, from a listing later pages answer for.
        if asked_head.is_some() && page.head_commit_id != asked_head {
            this.update(cx, |this, cx| {
                this.more = More::Failed {
                    error: "the server answered for another head".into(),
                    restart: true,
                };
                cx.notify();
            })
            .ok();
            return;
        }
        let Ok(held) = this.update(cx, |this, _| this.read.clone()) else {
            return;
        };
        let listing = Listing::of(&page);
        let complete = listing.complete;
        let arranged = cx
            .background_spawn(async move {
                let mut read = HistoryRead::clone(&held);
                read.next_page(page.commits, complete);
                arrange(read)
            })
            .await;
        let laid_out = this.update(cx, |this, cx| {
            this.next_page = listing.next;
            this.more = More::Idle;
            this.take(arranged, cx);
        });
        if laid_out.is_ok() {
            Self::read_merged(this, cx).await;
        }
    }

    /// Reads the next page once the rows drawn come within a screen of the list's end.
    fn want_more(&mut self, rows: &Range<usize>, cx: &mut Context<Self>) {
        if rows.end + rows.len() >= self.commits.len() {
            self.load_next(cx);
        }
    }

    /// Whether the list ends in a row for the next page: the listing goes on.
    fn has_more(&self) -> bool {
        !self.read.complete && !self.commits.is_empty()
    }

    /// Try again, after the next page failed: that page again, or the listing from its first
    /// page when the server refused the page's continuation.
    fn retry_more(&mut self, cx: &mut Context<Self>) {
        if let More::Failed { restart: true, .. } = self.more {
            self.load(false, cx);
        } else {
            self.more = More::Idle;
            self.load_next(cx);
        }
    }

    /// Goes on reading merged-in commits once the rows drawn name one not read yet, when
    /// nothing is being read: those a walk left for want of [`MERGED_READS`], read as the person
    /// scrolls to them, and those whose read failed [`MERGED_RETRY_AFTER`] ago, or whose
    /// branch's page, read for a token refused commits by id, failed so long ago. A walk asks
    /// for none it asked for within that time, so the walks end once the rows drawn name none.
    fn want_merged(&mut self, rows: &Range<usize>, cx: &mut Context<Self>) {
        if self._task.is_some() || !self.shown() {
            return;
        }
        // From the listings, which cannot be read before the branch list is.
        if !self.by_id && self.listed.is_none() && self.session.read(cx).branches.loaded().is_none()
        {
            return;
        }
        let now = cx.background_executor().now();
        let drawn = self
            .commits
            .get(rows.start..rows.end.min(self.commits.len()))
            .unwrap_or_default();
        let lists_due = !self.by_id && self.listed.as_ref().is_some_and(|l| l.due(now));
        let names_unread = drawn
            .iter()
            .flat_map(|c| c.parent_commit_id.iter().chain(&c.merged_parent_commit_id))
            .any(|id| {
                (self.wanted.contains(id) && self.may_ask(id, now))
                    || (lists_due && self.read.absent(id) == Some(Absent::Unlisted))
            });
        if names_unread {
            self._task = Some(cx.spawn(async move |this, cx| {
                Self::read_merged(&this, cx).await;
                this.update(cx, |this, _| this._task = None).ok();
            }));
        }
    }

    /// Whether merged-in commit `id` may be asked for at `now`: its read has not failed since
    /// the last page read, or failed [`MERGED_RETRY_AFTER`] ago or more.
    fn may_ask(&self, id: &str, now: Instant) -> bool {
        self.merged_failed
            .get(id)
            .is_none_or(|failed| now.saturating_duration_since(*failed) >= MERGED_RETRY_AFTER)
    }

    /// The merged-in commits to read next, at most `count`: those the rows drawn last name
    /// first, so a stub scrolled to does not wait for every merge above it, then the rest, top
    /// rows' first.
    fn merged_to_read(&self, count: usize, now: Instant) -> Vec<String> {
        let wanted: HashSet<&str> = self.wanted.iter().map(String::as_str).collect();
        let drawn = self.drawn_rows.clone().unwrap_or_default();
        let named = self
            .commits
            .get(drawn.start..drawn.end.min(self.commits.len()))
            .unwrap_or_default()
            .iter()
            .flat_map(|c| c.parent_commit_id.iter().chain(&c.merged_parent_commit_id))
            .map(String::as_str)
            .filter(|id| wanted.contains(id));
        let mut picked = Vec::new();
        let mut seen = HashSet::new();
        for id in named.chain(self.wanted.iter().map(String::as_str)) {
            if picked.len() == count {
                break;
            }
            if self.may_ask(id, now) && seen.insert(id) {
                picked.push(id.to_owned());
            }
        }
        picked
    }

    /// Draws the rows again once [`MERGED_RETRY_AFTER`] has passed since a merged-in read
    /// failed, so a drawn row naming it asks again ([`Self::want_merged`]) without waiting for a
    /// scroll. One wait at a time: a later failure's takes its place, and ends after it.
    fn retry_merged_later(&mut self, cx: &mut Context<Self>) {
        self._merged_retry = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(MERGED_RETRY_AFTER).await;
            this.update(cx, |_, cx| cx.notify()).ok();
        }));
    }

    /// Reads the merged-in commits the commits drawn name, by id, [`MERGED_AT_ONCE`] at a time
    /// and at most [`MERGED_READS`] in one walk ([`Self::merged_to_read`]), laying the history
    /// out again when a batch brings a commit among the rows read. A commit the server does not
    /// have draws as a stub from then on; one whose read failed otherwise is asked for again
    /// later. Once the server refuses this token a read by id, the rest come from the other
    /// branches' listings ([`Self::read_listed`]).
    async fn read_merged(this: &WeakEntity<Self>, cx: &mut AsyncApp) {
        let mut left = MERGED_READS;
        loop {
            let Ok(by_id) = this.update(cx, |this, _| this.by_id) else {
                return;
            };
            if !by_id {
                Self::read_listed(this, cx).await;
                return;
            }
            if left == 0 {
                return;
            }
            let Ok(reads) = this.update(cx, |this, cx| {
                let now = cx.background_executor().now();
                this.merged_to_read(MERGED_AT_ONCE.min(left), now)
                    .into_iter()
                    .map(|id| {
                        let read = this
                            .session
                            .update(cx, |session, cx| session.commit(&id, cx));
                        (id, read)
                    })
                    .collect::<Vec<_>>()
            }) else {
                return;
            };
            if reads.is_empty() {
                return;
            }
            left -= reads.len();
            // Sent together above; the answers are awaited in the order asked.
            let mut answers = Vec::with_capacity(reads.len());
            let mut failed = Vec::new();
            let mut refused = false;
            for (id, read) in reads {
                match read.await {
                    Ok(commit) => answers.push((id, Some(commit))),
                    // This token may not read commits by id: the listings answer this one too.
                    Err(ReadError::Refused(refusal)) if refusal.status == 403 => refused = true,
                    Err(ReadError::Refused(refusal))
                        if matches!(refusal.status, 400 | 404 | 410) =>
                    {
                        answers.push((id, None))
                    }
                    Err(_) => failed.push(id),
                }
            }
            let Ok(held) = this.update(cx, |this, cx| {
                if !failed.is_empty() {
                    let now = cx.background_executor().now();
                    this.merged_failed
                        .extend(failed.into_iter().map(|id| (id, now)));
                    this.retry_merged_later(cx);
                }
                if refused {
                    this.by_id = false;
                }
                this.read.clone()
            }) else {
                return;
            };
            if answers.is_empty() {
                continue;
            }
            let taken = cx
                .background_spawn(async move {
                    let mut read = HistoryRead::clone(&held);
                    // A batch of commits held back, or not had, changes no row drawn: the
                    // history is laid out again only for one that sorts among the rows read.
                    let shows = answers
                        .iter()
                        .any(|(_, commit)| commit.as_ref().is_some_and(|c| read.shows(c)));
                    let answered: Vec<String> = answers.iter().map(|(id, _)| id.clone()).collect();
                    for (id, commit) in answers {
                        read.merged_read(id, commit);
                    }
                    if shows {
                        Answered::Drawn(arrange(read))
                    } else {
                        Answered::Unseen(Arc::new(read), answered)
                    }
                })
                .await;
            let kept = this.update(cx, |this, cx| match taken {
                Answered::Drawn(arranged) => this.take(arranged, cx),
                Answered::Unseen(read, answered) => {
                    this.read = read;
                    this.wanted.retain(|id| !answered.contains(id));
                }
            });
            if kept.is_err() {
                return;
            }
        }
    }

    /// Answers the merged-in commits wanted from the other branches' listings, for a token the
    /// server refuses commits by id: the newest [`LISTED_PAGE`] commits of up to
    /// [`OTHER_BRANCHES`] live branches, one page each, read once into `listed`, then laid out
    /// once. A commit none of those pages holds draws as a stub, saying so: not listed
    /// ([`Absent::NotListed`]), or, while some branch's page could not be read, perhaps on that
    /// branch ([`Absent::Unlisted`]). A page that could not be read in passing is read again,
    /// it alone, once [`MERGED_RETRY_AFTER`] has passed ([`Self::want_merged`]).
    async fn read_listed(this: &WeakEntity<Self>, cx: &mut AsyncApp) {
        // The pages to read: every other branch's the first time, then those that failed in
        // passing, once their wait is over.
        let Ok(Some((mut listed, reads, now))) = this.update(cx, |this, cx| {
            let now = cx.background_executor().now();
            let names: Vec<String> = match &this.listed {
                None => {
                    let branch = this.branch.clone();
                    this.session
                        .read(cx)
                        .branches
                        .loaded()?
                        .iter()
                        .filter(|name| **name != branch)
                        .take(OTHER_BRANCHES)
                        .cloned()
                        .collect()
                }
                Some(listed) if listed.due(now) => listed.failed.clone(),
                Some(_) => Vec::new(),
            };
            let reads: Vec<_> = names
                .into_iter()
                .map(|name| {
                    let read = this.session.update(cx, |session, cx| {
                        session.commit_page(&name, LISTED_PAGE, None, cx)
                    });
                    (name, read)
                })
                .collect();
            Some((this.listed.clone().unwrap_or_default(), reads, now))
        }) else {
            // No branch list yet: the stubs stay, and a drawn row naming one walks again once
            // it is in.
            return;
        };
        if !reads.is_empty() {
            let mut lists = Vec::with_capacity(reads.len());
            let mut failed = Vec::new();
            for (name, read) in reads {
                match read.await {
                    Ok(page) => lists.push(page.commits),
                    Err(error) if refused_again(&error) => listed.refused.push(name),
                    Err(_) => failed.push(name),
                }
            }
            let held = listed.commits.clone();
            listed.commits = cx
                .background_spawn(async move {
                    let mut by_id = HashMap::clone(&held);
                    for commit in lists.iter().flat_map(|list| list.iter()) {
                        by_id
                            .entry(commit.graph_commit_id.clone())
                            .or_insert_with(|| commit.clone());
                    }
                    Arc::new(by_id)
                })
                .await;
            listed.failed_at = (!failed.is_empty()).then_some(now);
            listed.failed = failed;
            let stored = this.update(cx, |this, cx| {
                if !listed.failed.is_empty() {
                    this.retry_merged_later(cx);
                }
                this.listed = Some(listed.clone());
            });
            if stored.is_err() {
                return;
            }
        }
        let Ok(held) = this.update(cx, |this, _| this.read.clone()) else {
            return;
        };
        let arranged = cx
            .background_spawn(async move {
                let mut read = HistoryRead::clone(&held);
                read.take_from(&listed.commits, listed.complete());
                arrange(read)
            })
            .await;
        this.update(cx, |this, cx| this.take(arranged, cx)).ok();
    }

    /// Shows a layout of what was read, keeping the commit selected and the rows being read
    /// where they are ([`Self::keep_place`]).
    fn take(&mut self, arranged: Arranged, cx: &mut Context<Self>) {
        let previous = self
            .selected
            .and_then(|ix| self.commits.get(ix))
            .map(|c| c.graph_commit_id.clone());
        let place = self.reading_at();
        self.read = arranged.read;
        self.commits = arranged.commits;
        self.commit_texts = arranged.commit_texts;
        self.graph = arranged.graph;
        self.wanted = arranged.wanted;
        self.heads = self.branch_heads();
        // The drawn rows' counts were found in the list before.
        self.count_rows = None;
        self.keep_place(place);
        let keep =
            previous.and_then(|id| self.commits.iter().position(|c| c.graph_commit_id == id));
        match keep {
            Some(ix) => self.selected = Some(ix),
            None if !self.commits.is_empty() => self.select(0, cx),
            None => {}
        }
        // So were the matches: found again, a page may bring some, and placed at the selection,
        // so a step goes on from it.
        self.match_commits(cx);
        self.match_at = self.match_at_selection();
        cx.notify();
    }

    /// The commit at the top of the rows shown, by its id, and its row, when the list is
    /// scrolled from its top: the row being read. At the top, `None`.
    fn reading_at(&self) -> Option<(String, usize)> {
        let offset = -f32::from(self.scroll.0.borrow().base_handle.offset().y);
        if offset <= 0. {
            return None;
        }
        let row = (offset / ROW_HEIGHT).floor() as usize;
        Some((self.commits.get(row)?.graph_commit_id.clone(), row))
    }

    /// Keeps the rows being read where they are (constitution V): when rows came or went above
    /// the commit at the top of those shown, the list scrolls by as many, so that commit stays
    /// at the top. A list at its top shows the newest commits there.
    fn keep_place(&mut self, place: Option<(String, usize)>) {
        let Some((id, was)) = place else {
            return;
        };
        let Some(now) = self.commits.iter().position(|c| c.graph_commit_id == id) else {
            return;
        };
        if now == was {
            return;
        }
        let handle = self.scroll.0.borrow().base_handle.clone();
        let offset = handle.offset();
        let moved = (now as f32 - was as f32) * ROW_HEIGHT;
        handle.set_offset(point(offset.x, offset.y - px(moved)));
    }

    /// Where the selection sits among the matches: its own place when it is one, else the
    /// last match above it, or the last of all when none is, so a step forward lands on the
    /// first match below it.
    fn match_at_selection(&self) -> Option<usize> {
        let last = self.matches.len().checked_sub(1)?;
        let Some(selected) = self.selected else {
            return Some(last);
        };
        Some(match self.matches.binary_search(&selected) {
            Ok(at) => at,
            Err(0) => last,
            Err(below) => below - 1,
        })
    }

    /// Branch names by the commit each points at: this branch's at the newest commit its
    /// listing read, the others' at the heads read for them.
    fn branch_heads(&self) -> HashMap<String, Vec<String>> {
        let own = self
            .read
            .own
            .first()
            .map(|head| (self.branch.as_str(), head.graph_commit_id.as_str()));
        branch_heads(
            own.into_iter().chain(
                self.other_heads
                    .iter()
                    .map(|(branch, head)| (branch.as_str(), head.as_str())),
            ),
        )
    }

    /// Reads the other branches' heads for their tags: the live branches besides this one, up
    /// to [`OTHER_BRANCHES`], one commit each. A head already read is kept unless `fresh`; a
    /// branch no longer listed loses its tag.
    fn load_heads(&mut self, fresh: bool, cx: &mut Context<Self>) {
        let branch = self.branch.clone();
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
        self.other_heads.retain(|name, _| others.contains(name));
        self.head_asked.retain(|name, _| others.contains(name));
        self.head_reads.retain(|name, _| others.contains(name));
        let unread: Vec<String> = others
            .iter()
            .filter(|name| fresh || !self.head_asked.contains_key(*name))
            .cloned()
            .collect();
        self.others = others;
        self.heads = self.branch_heads();
        for name in unread {
            self.read_head(name, fresh, cx);
        }
        cx.notify();
    }

    /// Reads `branch`'s head for its tag: the one the session keeps
    /// ([`GraphSession::head_commit`]), which it reads again once a commit lands on the branch,
    /// or with `fresh` read again now, for every view that shows it.
    fn read_head(&mut self, branch: String, fresh: bool, cx: &mut Context<Self>) {
        self.next_head_read += 1;
        let asked = self.next_head_read;
        self.head_asked.insert(branch.clone(), asked);
        let read = self
            .session
            .update(cx, |session, cx| session.head_commit(&branch, fresh, cx));
        let name = branch.clone();
        let task = cx.spawn(async move |this, cx| {
            let head = read.await;
            this.update(cx, |this, cx| {
                if this.head_asked.get(&name) != Some(&asked) {
                    return;
                }
                this.head_reads.remove(&name);
                match head {
                    Ok(Some(head)) => {
                        this.other_heads.insert(name, head.graph_commit_id);
                    }
                    Ok(None) => {
                        this.other_heads.remove(&name);
                    }
                    // A branch whose head could not be read has no tag; the next reload asks.
                    Err(_) => {
                        this.other_heads.remove(&name);
                        this.head_asked.remove(&name);
                    }
                }
                this.heads = this.branch_heads();
                cx.notify();
            })
            .ok();
        });
        self.head_reads.insert(branch, task);
    }

    /// The search's counter: where the selection is among the matches, or that there are none.
    fn match_counter(&self) -> Option<String> {
        if self.match_query.is_empty() {
            return None;
        }
        Some(match self.match_at {
            Some(at) => format!("{}/{}", at + 1, self.matches.len()),
            None => "No match".to_owned(),
        })
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

    /// The commit "New branch here" starts a branch at: the selected one, when it is published.
    fn branch_here(&self) -> Option<&str> {
        branch_start(self.commits.get(self.selected?)?)
    }

    fn new_branch_here(&mut self, _: &NewBranchHere, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(commit) = self.branch_here() {
            new_branch_at(commit, window, cx);
        }
    }

    /// ↵ opens the selected commit's own tab, with the commit as listed.
    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(commit) = self.selected.and_then(|ix| self.commits.get(ix)) {
            open_listed(&self.session, commit, window, cx);
        }
    }

    fn graph_width(&self) -> f32 {
        ((self.graph.lane_count.max(1) as f32) * LANE_WIDTH + GRAPH_PADDING * 2.).max(52.)
    }

    /// Notes the rows a frame draws. The list also lays one row out twice a frame to measure
    /// it, so the rows are taken once the frame is out, as the last range it asked for: else
    /// every redraw would restart the pause before the counts are read. Rows drawn near the
    /// list's end read its next page, and rows naming a merged-in commit not read yet read it.
    fn drawn(&mut self, rows: Range<usize>, window: &mut Window, cx: &mut Context<Self>) {
        self.drawn_rows = Some(rows);
        if !std::mem::replace(&mut self.drawn_due, true) {
            cx.defer_in(window, |this, _, cx| {
                this.drawn_due = false;
                if let Some(rows) = this.drawn_rows.clone() {
                    this.want_more(&rows, cx);
                    this.want_merged(&rows, cx);
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
    /// changes never change, so an answer is kept for the life of the view. The graph's first
    /// commit has no parent to compare with, so its changes are not asked for.
    fn load_row_counts(&mut self, range: Range<usize>, cx: &mut Context<Self>) {
        let ids: Vec<String> = self
            .commits
            .get(range.start..range.end.min(self.commits.len()))
            .unwrap_or_default()
            .iter()
            .filter(|commit| !crate::commit_view::is_first_commit(commit))
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
                        // Shared with the session's memo: copied here, off the main thread.
                        fold.add_block(graph_client::api::ChangeBlockOutput {
                            cause: page.cause,
                            changes: Arc::unwrap_or_clone(page.changes),
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
        let more = (self.has_more() && range.contains(&self.commits.len()))
            .then(|| self.render_more(graph_width, cx));
        let mut rows: Vec<AnyElement> = range
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
                    stub: self.is_stub(ix),
                };
                let unnamed = commit
                    .merged_parent_commit_id
                    .as_deref()
                    .filter(|_| lines.stub)
                    .map(|merged| self.why_not_drawn(merged));
                let branch = commit
                    .graph_branch
                    .clone()
                    .unwrap_or_else(|| "main".to_owned());
                let id = commit.graph_commit_id.clone();
                let counts = self.counts.get(&id).copied();
                let partial = self.partial_counts.contains(&id);
                let first = crate::commit_view::is_first_commit(commit);
                let heads = self.heads.get(&id).cloned().unwrap_or_default();
                let selected = self.selected == Some(ix);
                let actor = commit.actor_id.clone();
                let menu_commit = commit.clone();
                let menu_session = self.session.clone();
                let menu_focus = self.focus_handle.clone();
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
                            // The first commit has nothing before it to count against.
                            .when(first, |d| {
                                d.child(
                                    studio_ui::ds::text::meta("first commit")
                                        .single_line()
                                        .truncate(),
                                )
                            })
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
                                let title = format!("Merged into {branch}");
                                d.child(
                                    div()
                                        .id(("merge", ix))
                                        // A stub says why its merged commits are not drawn.
                                        .tooltip(move |_, cx| match &unnamed {
                                            Some(why) => Tooltip::with_meta(
                                                title.clone(),
                                                None,
                                                why.clone(),
                                                cx,
                                            ),
                                            None => Tooltip::simple(title.clone(), cx),
                                        })
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
                            // The panel has no detail split: a click opens the commit's tab,
                            // with the commit as listed.
                            if (this.compact || event.click_count() == 2)
                                && let Some(commit) = this.commits.get(ix)
                            {
                                open_listed(&this.session, commit, window, cx);
                            }
                        }),
                    );
                Some(
                    right_click_menu(("commit-menu", ix))
                        .trigger(move |_, _, _| row_element)
                        .menu(move |window, cx| {
                            let (commit, focus) = (menu_commit.clone(), menu_focus.clone());
                            let session = menu_session.clone();
                            ContextMenu::build(window, cx, move |menu, _, _| {
                                commit_menu(menu.context(focus), &session, &commit)
                            })
                        })
                        .into_any_element(),
                )
            })
            .collect();
        rows.extend(more);
        rows
    }

    /// The row after the last commit read while the listing goes on: its next page being read,
    /// or why it could not be, with Try again.
    fn render_more(&self, graph_width: f32, cx: &mut Context<Self>) -> AnyElement {
        let status = match &self.more {
            More::Failed { error, .. } => h_flex()
                .min_w_0()
                .gap(px(studio_ui::ds::space::MD))
                .child(
                    div().min_w_0().overflow_hidden().child(
                        studio_ui::ds::text::muted(format!(
                            "More commits could not be read: {error}"
                        ))
                        .single_line()
                        .truncate(),
                    ),
                )
                .child(
                    studio_ui::ds::row_button("retry-more-commits", "Try again")
                        .style(ButtonStyle::Subtle)
                        .on_click(cx.listener(|this, _, _, cx| this.retry_more(cx))),
                ),
            More::Idle | More::Reading => h_flex()
                .gap(px(studio_ui::ds::space::SM))
                .child(studio_ui::ds::spinner())
                .child(studio_ui::ds::text::meta("Reading more commits…")),
        };
        h_flex()
            .id("more-commits")
            .h(px(ROW_HEIGHT))
            .mx(px(studio_ui::ds::space::SM))
            .pr(px(studio_ui::ds::space::MD))
            .gap(px(studio_ui::ds::space::LG))
            .child(div().w(px(graph_width)).h_full().flex_none())
            .child(status)
            .into_any_element()
    }

    /// Whether row `ix` is a merge whose merged parent is not drawn, so its lane ends in a
    /// stub.
    fn is_stub(&self, ix: usize) -> bool {
        let (Some(commit), Some(row)) = (self.commits.get(ix), self.graph.rows.get(ix)) else {
            return false;
        };
        commit.merged_parent_commit_id.is_some() && !row.outgoing.iter().any(|l| l.lane != row.lane)
    }

    /// Why the commits a merge brought in are not drawn below it, by its merged parent `id`: not
    /// had, not in the listings read, perhaps in a listing that could not be read, its read
    /// failed, not read yet, or older than the rows read.
    fn why_not_drawn(&self, id: &str) -> String {
        match self.read.absent(id) {
            Some(Absent::NotHad) => {
                "The server does not serve the merged commit, as for a deleted branch's latest \
                 commits and history cleaned up."
                    .into()
            }
            Some(Absent::NotListed) => format!(
                "The merged commit is not among the newest {} commits of any branch read, and \
                 this credential may not read a commit by its id.",
                group_digits(LISTED_PAGE as usize)
            ),
            Some(Absent::Unlisted) => self
                .listed
                .as_ref()
                .map(Listed::why_unlisted)
                .unwrap_or_else(|| "The merged commits are being read.".into()),
            None if self.merged_failed.contains_key(id) => {
                "The merged commit could not be read; it is asked for again while this row shows."
                    .into()
            }
            None if self.wanted.iter().any(|wanted| wanted == id) => {
                "The merged commits are being read.".into()
            }
            None => {
                "The merged commits are older than the commits read so far, and show as more are \
                 read."
                    .into()
            }
        }
    }

    fn render_list(&self, cx: &mut Context<Self>) -> AnyElement {
        // No column header: an id, tags, counts, an actor and a time say what they are.
        v_flex()
            .size_full()
            .pt(px(studio_ui::ds::space::SM))
            .child(
                uniform_list(
                    "history-rows",
                    self.commits.len() + usize::from(self.has_more()),
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

/// A commit row's menu. "Open in a tab" opens the commit as listed; "New branch here" shows the
/// key that starts a branch at the selected commit, and starts one at this row's; it is offered
/// only at a published commit.
fn commit_menu(
    menu: ContextMenu,
    session: &Entity<GraphSession>,
    commit: &CommitOutput,
) -> ContextMenu {
    let copy_id = commit.graph_commit_id.clone();
    let (session, listed) = (session.clone(), commit.clone());
    let branch_at = branch_start(commit).map(str::to_owned);
    menu.entry("Copy commit id", None, move |_, cx| {
        cx.write_to_clipboard(ClipboardItem::new_string(copy_id.clone()))
    })
    .entry("Open in a tab", None, move |window, cx| {
        open_listed(&session, &listed, window, cx)
    })
    .when_some(branch_at, |menu, commit| {
        menu.separator().entry(
            "New branch here…",
            Some(Box::new(NewBranchHere)),
            move |window, cx| new_branch_at(&commit, window, cx),
        )
    })
}

/// The toolbar's one fact about the commits read: how many, "so far" while the listing goes on
/// below them, and how many of them were merged in, each count grouped as Studio writes every
/// count.
fn summary(count: usize, merged: usize, complete: bool) -> String {
    let mut summary = format!(
        "{} commit{}",
        group_digits(count),
        if count == 1 { "" } else { "s" }
    );
    if !complete {
        summary.push_str(" so far");
    }
    if merged > 0 {
        summary.push_str(&format!(" · {} merged in", group_digits(merged)));
    }
    summary
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
        let counter = self.match_counter();
        let search = studio_ui::ds::search_field(self.search.clone(), cx)
            .w(px(if self.compact { 200. } else { 240. }))
            .children(counter.map(studio_ui::ds::text::meta));
        let summary = match self.state {
            State::Ready | State::Stale(_) => summary(
                count,
                count.saturating_sub(self.read.own.len()),
                self.read.complete,
            ),
            State::Loading => "Reading…".into(),
            State::Failed(_) => String::new(),
        };
        let stale = match &self.state {
            State::Stale(error) => Some(error.clone()),
            _ => None,
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
            // The commits shown stay as read when the first page read again fails: the failure
            // sits beside their count, the reason in its tooltip.
            .when_some(stale, |header, error| {
                header
                    .child(
                        div()
                            .id("history-read-again-failed")
                            .debug_selector(|| "history-read-again-failed".into())
                            .min_w_0()
                            .overflow_hidden()
                            .tooltip(Tooltip::text(format!(
                                "{error}. The commits shown are as read before."
                            )))
                            .child(
                                studio_ui::ds::text::error("The newest commits could not be read")
                                    .single_line()
                                    .truncate(),
                            ),
                    )
                    .child(
                        studio_ui::ds::toolbar_button("retry-history", "Try again")
                            .style(ButtonStyle::Subtle)
                            .tooltip(Tooltip::text(
                                "Read the branch's newest commits again; the commits read stay \
                                 below them",
                            ))
                            .on_click(cx.listener(|this, _, _, cx| this.retry(cx))),
                    )
            })
            .child(div().flex_1())
            .child(search)
            .child(
                studio_ui::ds::toolbar_icon_button("refresh-history", IconName::RotateCw)
                    .icon_color(Color::Muted)
                    .tooltip(|_, cx| Tooltip::for_action("Read the history again", &Refresh, cx))
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
            );
        let body = match &self.state {
            // Only while no commit is shown ([`Self::first_page`]).
            State::Failed(error) => {
                studio_ui::ds::error_state("History could not be read", error.clone())
                    .debug_selector(|| "history-error".into())
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
            // ⌘R reads the history again, and goes on to the workspace, which reads the graph's
            // schema, branches and counts again.
            .on_action(cx.listener(|this, _: &Refresh, _, cx| {
                this.refresh(cx);
                cx.propagate();
            }))
            .on_action(cx.listener(|this, _: &NextMatch, _, cx| this.step_match(true, cx)))
            .on_action(cx.listener(|this, _: &PreviousMatch, _, cx| this.step_match(false, cx)))
            .when(self.branch_here().is_some(), |view| {
                view.on_action(cx.listener(Self::new_branch_here))
            })
            .child(header)
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use gpui::{TestAppContext, VisualTestContext};
    use graph_store::{Connection, ServerEntry, ServerSource};
    use http_client::{AsyncBody, FakeHttpClient, Inner, Response};
    use serde_json::{Value, json};

    use super::*;

    /// A server for History's tests. It holds each branch's first-parent chain, newest first,
    /// and pages a listing as OmniGraph does (`limit`, `page_token`, `head_commit_id`; a later
    /// page answers for the head the first resolved). It answers a commit by its id, the head
    /// probe with the branch's newest commit, and every commit's changes with none, and notes
    /// every request.
    #[derive(Default)]
    struct Served {
        /// Each branch's chain, newest first, in the branch list's order.
        branches: Vec<(String, Vec<Value>)>,
        /// Commits only their id reaches: those of a branch not listed.
        loose: Vec<Value>,
        /// A read of a commit by id is refused (403), as for a token whose read grant names
        /// branches; and how many such reads fail (500) before one is answered.
        refuse_by_id: bool,
        fail_by_id: usize,
        /// A page token is refused (400), or its read fails (500).
        refuse_tokens: bool,
        fail_tokens: bool,
        /// Listings, by query, whose read fails (503, a server shedding load), and how many
        /// times more; and those refused (403, a branch the credential may not read).
        fail_listings: HashMap<String, usize>,
        refuse_listings: Vec<String>,
        /// Every request's path and query.
        asked: Vec<String>,
    }

    impl Served {
        fn chain(&self, branch: &str) -> &[Value] {
            self.branches
                .iter()
                .find(|(name, _)| name == branch)
                .map_or(&[], |(_, chain)| chain.as_slice())
        }

        fn chain_mut(&mut self, branch: &str) -> &mut Vec<Value> {
            &mut self
                .branches
                .iter_mut()
                .find(|(name, _)| name == branch)
                .expect("a branch served")
                .1
        }

        /// The listings asked for, by query.
        fn listings(&self) -> Vec<&str> {
            self.asked
                .iter()
                .filter_map(|asked| asked.strip_prefix("/graphs/knowledge/commits?"))
                .collect()
        }

        /// How many times commit `id` was read by its id.
        fn reads_of(&self, id: &str) -> usize {
            let path = format!("/graphs/knowledge/commits/{id}?");
            self.asked.iter().filter(|asked| **asked == path).count()
        }

        /// Whether the listing asked by `query` fails this time.
        fn fails_listing(&mut self, query: &str) -> bool {
            match self.fail_listings.get_mut(query) {
                Some(left) if *left > 0 => {
                    *left -= 1;
                    true
                }
                _ => false,
            }
        }

        fn answer(&mut self, path: &str, query: &str, body: &Value) -> (u16, Value) {
            let pairs: HashMap<&str, &str> = query
                .split('&')
                .filter_map(|pair| pair.split_once('='))
                .collect();
            if path.ends_with("/branches") {
                let names: Vec<&String> = self.branches.iter().map(|(name, _)| name).collect();
                (200, json!({ "branches": names }))
            } else if path.ends_with("/schema") {
                // A node type for the head probe to match.
                let schema = "node Person {\n  slug: String @key\n}\n";
                (200, json!({ "schema_source": schema }))
            } else if path.ends_with("/query") {
                let branch = body["branch"].as_str().unwrap_or("main");
                let head = self
                    .chain(branch)
                    .first()
                    .map(|c| c["graph_commit_id"].clone());
                (
                    200,
                    json!({
                        "query_name": body["name"], "target": { "branch": branch },
                        "row_count": 0, "columns": [], "rows": [], "graph_commit_id": head,
                    }),
                )
            } else if path.contains("/commits/") && path.ends_with("/changes") {
                let id = path.rsplit('/').nth(1).unwrap_or_default();
                let cause =
                    json!({ "graph_commit_id": id, "authored_branch": "main", "authored_at": 1 });
                (200, json!({ "cause": cause, "changes": [] }))
            } else if path.ends_with("/changes") {
                (
                    200,
                    json!({ "blocks": [], "cursor": "cursor", "caught_up": true }),
                )
            } else if path.ends_with("/commits")
                && pairs.contains_key("page_token")
                && self.refuse_tokens
            {
                (400, json!({ "error": "page token rejected" }))
            } else if path.ends_with("/commits")
                && pairs.contains_key("page_token")
                && self.fail_tokens
            {
                (500, json!({ "error": "busy" }))
            } else if path.ends_with("/commits") && self.refuse_listings.iter().any(|q| q == query)
            {
                (403, json!({ "error": "forbidden" }))
            } else if path.ends_with("/commits") && self.fails_listing(query) {
                (503, json!({ "error": "busy" }))
            } else if path.ends_with("/commits") {
                let branch = pairs.get("branch").copied().unwrap_or("main");
                let limit = pairs.get("limit").map(|limit| limit.parse().unwrap());
                (
                    200,
                    self.listing(branch, limit, pairs.get("page_token").copied()),
                )
            } else if path.contains("/commits/") && self.refuse_by_id {
                (403, json!({ "error": "forbidden" }))
            } else if path.contains("/commits/") && self.fail_by_id > 0 {
                self.fail_by_id -= 1;
                (500, json!({ "error": "busy" }))
            } else if let Some((_, id)) = path.rsplit_once("/commits/") {
                let found = self
                    .branches
                    .iter()
                    .flat_map(|(_, chain)| chain)
                    .chain(&self.loose)
                    .find(|commit| commit["graph_commit_id"] == id);
                match found {
                    Some(commit) => (200, commit.clone()),
                    None => (404, json!({ "error": "commit not found" })),
                }
            } else {
                (200, json!({}))
            }
        }

        /// A page of `branch`'s chain. A token names the head the first page resolved and how
        /// far below it the listing has come, so later pages answer for that head's chain.
        fn listing(&self, branch: &str, limit: Option<usize>, token: Option<&str>) -> Value {
            let chain = self.chain(branch);
            if limit.is_none() && token.is_none() {
                return json!({ "commits": chain });
            }
            let limit = limit.unwrap_or(100);
            let (head, offset) = match token {
                Some(token) => {
                    let (head, offset) = token.rsplit_once('_').expect("a token served");
                    (Some(head.to_owned()), offset.parse::<usize>().unwrap())
                }
                None => (
                    chain
                        .first()
                        .and_then(|c| c["graph_commit_id"].as_str())
                        .map(str::to_owned),
                    0,
                ),
            };
            let at = head
                .as_deref()
                .and_then(|head| chain.iter().position(|c| c["graph_commit_id"] == head))
                .unwrap_or(chain.len());
            let start = (at + offset).min(chain.len());
            let end = (start + limit).min(chain.len());
            let next = (end < chain.len())
                .then(|| format!("{}_{}", head.as_deref().unwrap_or_default(), offset + limit));
            json!({ "commits": &chain[start..end], "head_commit_id": head, "next_page_token": next })
        }
    }

    fn commit(id: &str, at: i64, parent: Option<&str>, merged: Option<&str>) -> Value {
        json!({
            "graph_commit_id": id, "graph_manifest_version": at, "kind": "published", "created_at": at,
            "parent_commit_id": parent, "merged_parent_commit_id": merged,
        })
    }

    /// A chain of `n` commits, newest first: `{prefix}{n-1}` down to `{prefix}0`.
    fn chain(prefix: &str, n: usize) -> Vec<Value> {
        (0..n)
            .rev()
            .map(|i| {
                let parent = i.checked_sub(1).map(|p| format!("{prefix}{p}"));
                commit(
                    &format!("{prefix}{i}"),
                    10 * i as i64 + 10,
                    parent.as_deref(),
                    None,
                )
            })
            .collect()
    }

    /// A session on a graph `served` holds, with the theme and editor a view needs.
    fn serve(
        served: Served,
        cx: &mut TestAppContext,
    ) -> (Entity<GraphSession>, Arc<Mutex<Served>>) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let served = Arc::new(Mutex::new(served));
        let state = served.clone();
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            let query = request.uri().query().unwrap_or_default().to_owned();
            let body: Value = match &request.body().0 {
                Inner::Bytes(bytes) => serde_json::from_slice(bytes.get_ref()).unwrap_or_default(),
                _ => Value::Null,
            };
            let state = state.clone();
            async move {
                let (status, body) = {
                    let mut served = state.lock().unwrap();
                    served.asked.push(format!("{path}?{query}"));
                    served.answer(&path, &query, &body)
                };
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
            cx.new(|cx| GraphSession::new(connection, "knowledge".into(), "main".into(), cx));
        (session, served)
    }

    /// `main`'s History tab, read as far as it goes without scrolling.
    fn open<'a>(
        session: &Entity<GraphSession>,
        cx: &'a mut TestAppContext,
    ) -> (Entity<HistoryView>, &'a mut VisualTestContext) {
        let session = session.clone();
        let (view, cx) = cx.add_window_view(move |window, cx| {
            HistoryView::new(session, "main".into(), window, cx)
        });
        cx.run_until_parked();
        (view, cx)
    }

    fn ids(view: &HistoryView) -> Vec<&str> {
        view.commits
            .iter()
            .map(|c| c.graph_commit_id.as_str())
            .collect()
    }

    /// The toolbar's counts are grouped as every count Studio writes.
    #[test]
    fn the_summary_groups_its_counts() {
        assert_eq!(summary(1, 0, true), "1 commit");
        assert_eq!(
            summary(12_480, 1_200, false),
            "12,480 commits so far · 1,200 merged in"
        );
    }

    /// History lays out its branch's newest page, and reads the next when the rows drawn come
    /// within a screen of the end, continuing the listing the first page resolved; once the
    /// listing ends, nothing more is read. No listing is ever read whole.
    #[gpui::test]
    async fn history_reads_its_branch_a_page_at_a_time(cx: &mut TestAppContext) {
        let (session, served) = serve(
            Served {
                branches: vec![("main".into(), chain("m", 450))],
                ..Default::default()
            },
            cx,
        );
        let (view, cx) = open(&session, cx);
        view.read_with(cx, |view, _| {
            assert_eq!(view.commits.len(), PAGE as usize);
            assert_eq!(view.commits[0].graph_commit_id, "m449");
            assert_eq!(view.heads.get("m449"), Some(&vec!["main".to_owned()]));
            assert!(view.has_more());
        });
        assert_eq!(served.lock().unwrap().listings(), ["branch=main&limit=200"]);
        // Rows a screen or more from the end read nothing; rows near it read the next page.
        view.update(cx, |view, cx| view.want_more(&(0..40), cx));
        cx.run_until_parked();
        assert_eq!(served.lock().unwrap().listings().len(), 1);
        view.update(cx, |view, cx| view.want_more(&(170..200), cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.commits.len(), 400);
            assert_eq!(view.selected, Some(0), "the selection stays");
        });
        view.update(cx, |view, cx| view.want_more(&(380..401), cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.commits.len(), 450);
            assert_eq!(view.commits.last().unwrap().graph_commit_id, "m0");
            assert!(view.read.complete);
            assert!(!view.has_more());
        });
        view.update(cx, |view, cx| view.want_more(&(430..450), cx));
        cx.run_until_parked();
        assert_eq!(
            served.lock().unwrap().listings(),
            [
                "branch=main&limit=200",
                "branch=main&limit=200&page_token=m449_200",
                "branch=main&limit=200&page_token=m449_400",
            ]
        );
    }

    /// A drawn row's count is read from its commit's changes, except the graph's first commit:
    /// it has no parent, the server cannot compare it with anything (a 409), and its row names
    /// it instead of asking.
    #[gpui::test]
    async fn the_first_commits_changes_are_never_asked_for(cx: &mut TestAppContext) {
        let (session, served) = serve(
            Served {
                branches: vec![("main".into(), chain("m", 3))],
                ..Default::default()
            },
            cx,
        );
        let (view, cx) = open(&session, cx);
        view.update(cx, |view, cx| view.load_row_counts(0..3, cx));
        cx.run_until_parked();
        let changes = |id: &str| {
            let path = format!("/graphs/knowledge/commits/{id}/changes?");
            served
                .lock()
                .unwrap()
                .asked
                .iter()
                .filter(|asked| asked.starts_with(&path))
                .count()
        };
        assert_eq!(
            changes("m0"),
            0,
            "the first commit's changes were asked for"
        );
        assert!(changes("m1") > 0 && changes("m2") > 0);
        view.read_with(cx, |view, _| {
            assert!(view.counts.contains_key("m1") && view.counts.contains_key("m2"));
            assert!(!view.counts.contains_key("m0"));
            assert!(crate::commit_view::is_first_commit(&view.commits[2]));
            assert!(!crate::commit_view::is_first_commit(&view.commits[1]));
        });
    }

    /// A commit landing on the branch reads its first page again: the commits above the head
    /// the pages were read from go on top of them, and the next page still continues their
    /// listing. The rows being read stay where they are: the list scrolls by the rows that came
    /// above them, unless it is at its top, where the new ones show.
    #[gpui::test]
    async fn a_commit_landing_on_the_branch_keeps_the_pages_read(cx: &mut TestAppContext) {
        let (session, served) = serve(
            Served {
                branches: vec![("main".into(), chain("m", 450))],
                ..Default::default()
            },
            cx,
        );
        let (view, cx) = open(&session, cx);
        view.update(cx, |view, cx| view.want_more(&(170..200), cx));
        cx.run_until_parked();
        // Read from row 50 down, m399 at the top of the rows shown.
        let scrolled_to = |view: &Entity<HistoryView>, rows: f32, cx: &mut VisualTestContext| {
            view.update(cx, |view, _| {
                let mut scroll = view.scroll.0.borrow_mut();
                scroll.deferred_scroll_to_item = None;
                scroll
                    .base_handle
                    .set_offset(point(px(0.), px(-rows * ROW_HEIGHT)));
            })
        };
        let top_row = |view: &Entity<HistoryView>, cx: &mut VisualTestContext| {
            view.read_with(cx, |view, _| {
                -f32::from(view.scroll.0.borrow().base_handle.offset().y) / ROW_HEIGHT
            })
        };
        scrolled_to(&view, 50., cx);
        assert_eq!(
            view.read_with(cx, |view, _| view.reading_at()),
            Some(("m399".to_owned(), 50))
        );
        // Another writer commits; Studio hears of it at the next head read.
        session
            .update(cx, |session, cx| session.head_now("main", cx))
            .await;
        served
            .lock()
            .unwrap()
            .chain_mut("main")
            .insert(0, commit("m450", 4_510, Some("m449"), None));
        session
            .update(cx, |session, cx| session.head_now("main", cx))
            .await;
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.commits.len(), 401);
            assert_eq!(view.commits[0].graph_commit_id, "m450");
            assert_eq!(view.heads.get("m450"), Some(&vec!["main".to_owned()]));
            assert_eq!(view.heads.get("m449"), None);
            assert_eq!(view.commits[51].graph_commit_id, "m399");
        });
        assert_eq!(
            top_row(&view, cx),
            51.,
            "m399 stays at the top of the rows shown"
        );

        // At the top, the newest commit shows there.
        scrolled_to(&view, 0., cx);
        served
            .lock()
            .unwrap()
            .chain_mut("main")
            .insert(0, commit("m451", 4_520, Some("m450"), None));
        session
            .update(cx, |session, cx| session.head_now("main", cx))
            .await;
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.commits[0].graph_commit_id, "m451")
        });
        assert_eq!(top_row(&view, cx), 0.);
        view.update(cx, |view, cx| view.want_more(&(380..403), cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.commits.len(), 452);
            assert!(view.read.complete);
        });
        assert_eq!(
            served.lock().unwrap().listings(),
            [
                "branch=main&limit=200",
                "branch=main&limit=200&page_token=m449_200",
                "branch=main&limit=200",
                "branch=main&limit=200",
                "branch=main&limit=200&page_token=m449_400",
            ]
        );
    }

    /// The first page read again for a commit landing, failing while commits are shown (the
    /// server timed out, shed load, was busy), keeps them as read: the toolbar says why beside
    /// their count and offers Try again, and their pages go on being read below them. Recovery,
    /// by the branch list read again or Try again, reads the first page again from the head and
    /// keeps the pages read below it.
    #[gpui::test]
    async fn a_failed_read_again_keeps_the_commits_shown(cx: &mut TestAppContext) {
        let (session, served) = serve(
            Served {
                branches: vec![("main".into(), chain("m", 450))],
                ..Default::default()
            },
            cx,
        );
        let (view, cx) = open(&session, cx);
        view.update(cx, |view, cx| view.want_more(&(170..200), cx));
        cx.run_until_parked();
        let first_page = "branch=main&limit=200";
        // Another writer commits m450; the first page read for it fails.
        session
            .update(cx, |session, cx| session.head_now("main", cx))
            .await;
        {
            let mut served = served.lock().unwrap();
            served
                .chain_mut("main")
                .insert(0, commit("m450", 4_510, Some("m449"), None));
            served.fail_listings.insert(first_page.into(), usize::MAX);
        }
        session
            .update(cx, |session, cx| session.head_now("main", cx))
            .await;
        cx.run_until_parked();
        let stale = |view: &HistoryView| matches!(view.state, State::Stale(_));
        view.read_with(cx, |view, _| {
            assert!(stale(view));
            assert_eq!(view.commits.len(), 400);
            assert_eq!(view.commits[0].graph_commit_id, "m449");
            assert_eq!(view.selected, Some(0));
        });
        assert!(cx.debug_bounds("history-read-again-failed").is_some());
        assert!(
            cx.debug_bounds("history-error").is_none(),
            "the rows gave their place to the error"
        );
        // Their next page is read as before, continuing the listing they were read from.
        view.update(cx, |view, cx| view.want_more(&(380..401), cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(stale(view));
            assert_eq!(view.commits.len(), 450);
            assert!(view.read.complete);
        });
        // The branch list read again tries the first page again, which fails again: the
        // commits stay.
        view.update(cx, |view, cx| view.branches_changed(cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(stale(view));
            assert_eq!(view.commits.len(), 450);
        });
        // Try again reads it; it reaches the head the pages were read from, so m450 goes on top
        // of them.
        served.lock().unwrap().fail_listings.clear();
        view.update(cx, |view, cx| view.retry(cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(matches!(view.state, State::Ready));
            assert_eq!(view.commits.len(), 451);
            assert_eq!(view.commits[0].graph_commit_id, "m450");
            assert_eq!(view.commits[450].graph_commit_id, "m0");
            assert!(view.read.complete);
        });
        assert!(cx.debug_bounds("history-read-again-failed").is_none());
        assert_eq!(
            served.lock().unwrap().listings(),
            [
                first_page,
                "branch=main&limit=200&page_token=m449_200",
                first_page,
                "branch=main&limit=200&page_token=m449_400",
                first_page,
                first_page,
            ]
        );
    }

    /// A first read that fails, with no commit shown, says why in the list's place; the branch
    /// list read again reads the history from its first page.
    #[gpui::test]
    async fn a_first_read_that_fails_says_why_in_place_of_the_list(cx: &mut TestAppContext) {
        let first_page = "branch=main&limit=200";
        let (session, served) = serve(
            Served {
                branches: vec![("main".into(), chain("m", 3))],
                fail_listings: HashMap::from([(first_page.to_owned(), usize::MAX)]),
                ..Default::default()
            },
            cx,
        );
        let (view, cx) = open(&session, cx);
        view.read_with(cx, |view, _| {
            assert!(matches!(view.state, State::Failed(_)));
            assert!(view.commits.is_empty());
        });
        assert!(cx.debug_bounds("history-error").is_some());
        assert!(cx.debug_bounds("history-read-again-failed").is_none());
        served.lock().unwrap().fail_listings.clear();
        view.update(cx, |view, cx| view.branches_changed(cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(matches!(view.state, State::Ready));
            assert_eq!(ids(view), ["m2", "m1", "m0"]);
        });
        assert!(cx.debug_bounds("history-error").is_none());
    }

    /// Commits merged in are read one at a time by their ids, as each names the next, and join
    /// the history in time order with their lanes. A merged parent the server does not have
    /// stays a stub. A Refresh (⌘R, where the history has the keyboard) reads none of those
    /// read again, since a commit never changes, and asks again for the one the server did not
    /// have.
    #[gpui::test]
    async fn merged_in_commits_are_read_by_their_ids(cx: &mut TestAppContext) {
        let (session, served) = serve(
            Served {
                branches: vec![(
                    "main".into(),
                    vec![
                        commit("m3", 30, Some("m2"), Some("b2")),
                        commit("m2", 20, Some("m1"), Some("gone")),
                        commit("m1", 10, None, None),
                    ],
                )],
                loose: vec![
                    commit("b2", 25, Some("b1"), None),
                    commit("b1", 15, Some("m1"), None),
                ],
                ..Default::default()
            },
            cx,
        );
        let (view, cx) = open(&session, cx);
        let check = |view: &HistoryView| {
            assert_eq!(ids(view), ["m3", "b2", "m2", "b1", "m1"]);
            assert_eq!(view.read.own.len(), 3);
            // m3's merge opens a lane that b1 closes into m1.
            assert_eq!(view.graph.lane_count, 2);
            assert_eq!(view.graph.rows[0].outgoing.len(), 2);
            assert_eq!(view.graph.rows[4].incoming.len(), 2);
            // m2's merged parent is not drawn: its merge is a stub.
            let m2 = &view.graph.rows[2];
            assert!(m2.outgoing.iter().all(|line| line.lane == m2.lane));
        };
        view.read_with(cx, |view, _| {
            check(view);
            assert!(view.is_stub(2));
            assert_eq!(
                view.why_not_drawn("gone"),
                "The server does not serve the merged commit, as for a deleted branch's latest \
                 commits and history cleaned up."
            );
        });
        let reads = |served: &Arc<Mutex<Served>>| {
            let served = served.lock().unwrap();
            ["b2", "b1", "gone"].map(|id| served.reads_of(id))
        };
        assert_eq!(reads(&served), [1, 1, 1]);
        // ⌘R where the history has the keyboard is its Refresh.
        let listings = served.lock().unwrap().listings().len();
        cx.update(|window, cx| window.focus(&view.focus_handle(cx), cx));
        cx.dispatch_action(Refresh);
        cx.run_until_parked();
        view.read_with(cx, |view, _| check(view));
        assert_eq!(reads(&served), [1, 1, 2]);
        assert_eq!(
            served.lock().unwrap().listings().len(),
            listings + 1,
            "the first page read again"
        );
    }

    /// A merge whose branch goes on below the oldest commit of the pages read is read as far
    /// as those pages, and goes on with the next page.
    #[gpui::test]
    async fn a_merged_branch_is_read_as_far_as_the_pages_read(cx: &mut TestAppContext) {
        let mut main = chain("m", 250);
        // m240 merged x2, which forked from m0 before x1.
        main[9] = commit("m240", 2_410, Some("m239"), Some("x2"));
        let (session, served) = serve(
            Served {
                branches: vec![("main".into(), main)],
                loose: vec![
                    commit("x2", 2_405, Some("x1"), None),
                    commit("x1", 15, Some("m0"), None),
                ],
                ..Default::default()
            },
            cx,
        );
        let (view, cx) = open(&session, cx);
        view.read_with(cx, |view, _| {
            assert!(ids(view).contains(&"x2"));
            assert!(!ids(view).contains(&"x1"), "below the pages read");
            assert_eq!(view.commits.len(), PAGE as usize + 1);
        });
        view.update(cx, |view, cx| view.want_more(&(180..201), cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.commits.len(), 252);
            assert_eq!(&ids(view)[249..], ["m1", "x1", "m0"]);
            assert_eq!(view.graph.lane_count, 2);
        });
        let served = served.lock().unwrap();
        assert_eq!([served.reads_of("x2"), served.reads_of("x1")], [1, 1]);
    }

    /// One walk reads at most [`MERGED_READS`] merged-in commits; the rest are read once the rows
    /// naming them are drawn.
    #[gpui::test]
    async fn a_long_merged_branch_is_read_as_its_rows_are_drawn(cx: &mut TestAppContext) {
        let mut merged = chain("x", 205);
        merged[204] = commit("x0", 10, Some("m0"), None);
        let (session, served) = serve(
            Served {
                branches: vec![(
                    "main".into(),
                    vec![
                        commit("m1", 100_000, Some("m0"), Some("x204")),
                        commit("m0", 1, None, None),
                    ],
                )],
                loose: merged,
                ..Default::default()
            },
            cx,
        );
        let (view, cx) = open(&session, cx);
        let by_id = |served: &Arc<Mutex<Served>>| {
            let served = served.lock().unwrap();
            (0..205)
                .map(|i| served.reads_of(&format!("x{i}")))
                .collect::<Vec<_>>()
        };
        view.read_with(cx, |view, _| {
            assert_eq!(view.commits.len(), 2 + MERGED_READS);
            assert_eq!(view.wanted, ["x4"]);
        });
        let reads = by_id(&served);
        assert_eq!(reads[5..], [1; 200]);
        assert_eq!(reads[..5], [0; 5]);
        // The rows at the end of what was read name x4.
        view.update(cx, |view, cx| view.want_merged(&(190..202), cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.commits.len(), 207);
            assert!(view.wanted.is_empty());
            assert_eq!(view.graph.lane_count, 2);
        });
        assert_eq!(by_id(&served), [1; 205]);
    }

    /// A token whose read grant names branches is refused commits by id: merged-in commits then
    /// come from the other branches' listings, the newest page of each, never read whole. One
    /// none of those pages holds is a stub that says so: here `long`'s `z1`, below its newest
    /// 1,000 commits.
    #[gpui::test]
    async fn a_token_refused_commits_by_id_reads_merges_from_the_listings(cx: &mut TestAppContext) {
        let mut long = chain("l", LISTED_PAGE as usize + 100);
        let oldest = long.len() - 1;
        long[oldest]["parent_commit_id"] = json!("z1");
        long.extend([
            commit("z1", 15, Some("m1"), None),
            commit("m1", 10, None, None),
        ]);
        let (session, served) = serve(
            Served {
                branches: vec![
                    (
                        "main".into(),
                        vec![
                            commit("m3", 30, Some("m2"), Some("b2")),
                            commit("m2", 20, Some("m1"), Some("z1")),
                            commit("m1", 10, None, None),
                        ],
                    ),
                    (
                        "feature".into(),
                        vec![
                            commit("b3", 35, Some("b2"), None),
                            commit("b2", 25, Some("b1"), None),
                            commit("b1", 15, Some("m1"), None),
                            commit("m1", 10, None, None),
                        ],
                    ),
                    ("long".into(), long),
                ],
                refuse_by_id: true,
                ..Default::default()
            },
            cx,
        );
        let (view, cx) = open(&session, cx);
        view.read_with(cx, |view, _| {
            assert_eq!(ids(view), ["m3", "b2", "m2", "b1", "m1"]);
            assert_eq!(view.graph.lane_count, 2);
            let m2 = &view.graph.rows[2];
            assert!(
                m2.outgoing.iter().all(|line| line.lane == m2.lane),
                "a stub"
            );
            assert!(view.is_stub(2) && !view.is_stub(0));
            assert_eq!(
                view.why_not_drawn("z1"),
                "The merged commit is not among the newest 1,000 commits of any branch read, and \
                 this credential may not read a commit by its id."
            );
            assert!(!view.by_id);
        });
        let pages = |served: &Arc<Mutex<Served>>| {
            let served = served.lock().unwrap();
            let listings = served.listings();
            assert!(
                listings.iter().all(|listing| listing.contains("&limit=")),
                "a listing read whole: {listings:?}"
            );
            ["feature", "long"].map(|branch| {
                let page = format!("branch={branch}&limit={LISTED_PAGE}");
                listings.iter().filter(|listing| **listing == page).count()
            })
        };
        assert_eq!(pages(&served), [1, 1]);
        // A Refresh tries by id again, is refused again, and reads the pages again, as it reads
        // every head again.
        view.update(cx, |view, cx| view.refresh(cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert_eq!(ids(view).len(), 5));
        assert_eq!(pages(&served), [2, 2]);
    }

    /// For a token refused commits by id, a branch whose page could not be read is not taken for
    /// one without the merged commit: the stub says that branch's list could not be read. Once
    /// a while has passed, while the row shows, a page that failed in passing is read again, it
    /// alone, and draws the merge; one the server refused is read again only by a Refresh.
    #[gpui::test]
    async fn a_listing_that_could_not_be_read_is_read_again_alone(cx: &mut TestAppContext) {
        let (session, served) = serve(
            Served {
                branches: vec![
                    (
                        "main".into(),
                        vec![
                            commit("m3", 30, Some("m2"), Some("b2")),
                            commit("m2", 20, Some("m1"), Some("z9")),
                            commit("m1", 10, None, None),
                        ],
                    ),
                    (
                        "feature".into(),
                        vec![
                            commit("b2", 25, Some("b1"), None),
                            commit("b1", 15, Some("m1"), None),
                            commit("m1", 10, None, None),
                        ],
                    ),
                    (
                        "fix".into(),
                        vec![
                            commit("f1", 12, Some("m1"), None),
                            commit("m1", 10, None, None),
                        ],
                    ),
                    ("secret".into(), vec![commit("m1", 10, None, None)]),
                ],
                refuse_by_id: true,
                fail_listings: HashMap::from([(format!("branch=feature&limit={LISTED_PAGE}"), 1)]),
                refuse_listings: vec![format!("branch=secret&limit={LISTED_PAGE}")],
                ..Default::default()
            },
            cx,
        );
        let (view, cx) = open(&session, cx);
        view.read_with(cx, |view, _| {
            assert_eq!(ids(view), ["m3", "m2", "m1"]);
            assert!(view.is_stub(0) && view.is_stub(1));
            assert_eq!(view.read.absent("b2"), Some(Absent::Unlisted));
            assert_eq!(
                view.why_not_drawn("b2"),
                "The lists of branches feature, secret could not be read, and the merged commit \
                 is not among the newest 1,000 commits of the other branches read. It is asked \
                 for again while this row shows."
            );
        });
        let pages = |served: &Arc<Mutex<Served>>| {
            let served = served.lock().unwrap();
            let listings = served.listings();
            ["feature", "fix", "secret"].map(|branch| {
                let page = format!("branch={branch}&limit={LISTED_PAGE}");
                listings.iter().filter(|listing| **listing == page).count()
            })
        };
        assert_eq!(pages(&served), [1, 1, 1]);
        cx.executor()
            .advance_clock(MERGED_RETRY_AFTER - Duration::from_millis(1));
        cx.run_until_parked();
        view.update(cx, |view, cx| view.want_merged(&(0..3), cx));
        cx.run_until_parked();
        assert_eq!(pages(&served), [1, 1, 1], "read again too soon");
        cx.executor().advance_clock(Duration::from_millis(1));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(ids(view), ["m3", "b2", "m2", "b1", "m1"]);
            assert!(!view.is_stub(0) && view.is_stub(2));
            assert_eq!(view.read.absent("b2"), None);
            assert_eq!(
                view.why_not_drawn("z9"),
                "Branch secret's list could not be read, and the merged commit is not among the \
                 newest 1,000 commits of the other branches read. Refresh asks for it again."
            );
        });
        assert_eq!(
            pages(&served),
            [2, 1, 1],
            "only feature's page is read again"
        );
        // Nothing is left to read again in passing.
        cx.executor().advance_clock(MERGED_RETRY_AFTER);
        cx.run_until_parked();
        view.update(cx, |view, cx| view.want_merged(&(0..5), cx));
        cx.run_until_parked();
        assert_eq!(pages(&served), [2, 1, 1]);
    }

    /// A merged-in commit whose read failed is asked for again once a while has passed, while a
    /// row naming it is drawn, and not before.
    #[gpui::test]
    async fn a_failed_merged_read_is_asked_again_after_a_while(cx: &mut TestAppContext) {
        let (session, served) = serve(
            Served {
                branches: vec![(
                    "main".into(),
                    vec![
                        commit("m2", 20, Some("m1"), Some("b1")),
                        commit("m1", 10, None, None),
                    ],
                )],
                loose: vec![commit("b1", 15, Some("m1"), None)],
                fail_by_id: 1,
                ..Default::default()
            },
            cx,
        );
        let (view, cx) = open(&session, cx);
        let reads = |served: &Arc<Mutex<Served>>| served.lock().unwrap().reads_of("b1");
        view.read_with(cx, |view, _| assert_eq!(ids(view), ["m2", "m1"]));
        assert_eq!(reads(&served), 1);
        cx.executor()
            .advance_clock(MERGED_RETRY_AFTER - Duration::from_millis(1));
        cx.run_until_parked();
        view.update(cx, |view, cx| view.want_merged(&(0..2), cx));
        cx.run_until_parked();
        assert_eq!(reads(&served), 1, "asked again too soon");
        cx.executor().advance_clock(Duration::from_millis(1));
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert_eq!(ids(view), ["m2", "b1", "m1"]));
        assert_eq!(reads(&served), 2);
    }

    /// The merged-in commits the rows drawn name are read first, then the rest top rows' first.
    #[gpui::test]
    async fn merged_reads_go_to_the_rows_drawn_first(cx: &mut TestAppContext) {
        let main: Vec<Value> = (0..12)
            .rev()
            .map(|i| commit(&format!("m{i}"), 10 * i + 10, None, Some(&format!("p{i}"))))
            .collect();
        let (session, _served) = serve(
            Served {
                branches: vec![("main".into(), main)],
                ..Default::default()
            },
            cx,
        );
        let (view, cx) = open(&session, cx);
        view.update(cx, |view, cx| {
            // None of the merged parents is served: they stand for parents still to read.
            view.wanted = (0..12).rev().map(|i| format!("p{i}")).collect();
            view.drawn_rows = Some(10..12);
            let now = cx.background_executor().now();
            assert_eq!(
                view.merged_to_read(4, now),
                ["p1", "p0", "p11", "p10"],
                "rows 10 and 11 are m1 and m0"
            );
        });
    }

    /// Matches a later page brings are found, and the counter and a step go on from the
    /// selection.
    #[gpui::test]
    async fn a_search_finds_matches_a_later_page_brings(cx: &mut TestAppContext) {
        let mut main = chain("m", 450);
        // m100, on the second page.
        main[349]["actor_id"] = json!("act-dev");
        let (session, _served) = serve(
            Served {
                branches: vec![("main".into(), main)],
                ..Default::default()
            },
            cx,
        );
        let (view, cx) = open(&session, cx);
        view.update_in(cx, |view, window, cx| {
            view.search
                .update(cx, |editor, cx| editor.set_text("dev", window, cx))
        });
        cx.executor().advance_clock(SEARCH_DELAY);
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.match_counter().as_deref(), Some("No match"));
        });
        view.update(cx, |view, cx| view.want_more(&(170..200), cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.matches, [349]);
            assert_eq!(view.match_counter().as_deref(), Some("1/1"));
        });
        view.update(cx, |view, cx| view.step_match(true, cx));
        view.read_with(cx, |view, _| {
            assert_eq!(view.selected, Some(349));
            assert_eq!(view.commits[349].graph_commit_id, "m100");
        });
    }

    /// Try again after a refused continuation reads the listing from its first page; after one
    /// that failed otherwise it sends the same continuation again.
    #[gpui::test]
    async fn try_again_after_a_refused_page_starts_from_the_first(cx: &mut TestAppContext) {
        let (session, served) = serve(
            Served {
                branches: vec![("main".into(), chain("m", 450))],
                fail_tokens: true,
                ..Default::default()
            },
            cx,
        );
        let (view, cx) = open(&session, cx);
        view.update(cx, |view, cx| view.want_more(&(170..200), cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(matches!(view.more, More::Failed { restart: false, .. }));
        });
        {
            let mut served = served.lock().unwrap();
            served.fail_tokens = false;
            served.refuse_tokens = true;
        }
        view.update(cx, |view, cx| view.retry_more(cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(matches!(view.more, More::Failed { restart: true, .. }));
        });
        view.update(cx, |view, cx| view.retry_more(cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(matches!(view.more, More::Idle));
            assert_eq!(view.commits.len(), 200);
        });
        assert_eq!(
            served.lock().unwrap().listings(),
            [
                "branch=main&limit=200",
                "branch=main&limit=200&page_token=m449_200",
                "branch=main&limit=200&page_token=m449_200",
                "branch=main&limit=200",
            ]
        );
    }

    /// Other branches' tags sit at each one's newest commit, read alone; a branch with no
    /// commit has none.
    #[gpui::test]
    async fn tags_sit_at_each_branchs_newest_commit(cx: &mut TestAppContext) {
        let (session, served) = serve(
            Served {
                branches: vec![
                    (
                        "main".into(),
                        vec![
                            commit("m2", 20, Some("m1"), None),
                            commit("m1", 10, None, None),
                        ],
                    ),
                    (
                        "feature".into(),
                        vec![
                            commit("m2", 20, Some("m1"), None),
                            commit("m1", 10, None, None),
                        ],
                    ),
                    (
                        "fix".into(),
                        vec![
                            commit("f1", 15, Some("m1"), None),
                            commit("m1", 10, None, None),
                        ],
                    ),
                    ("empty".into(), Vec::new()),
                ],
                ..Default::default()
            },
            cx,
        );
        let (view, cx) = open(&session, cx);
        view.read_with(cx, |view, _| {
            assert_eq!(view.heads["m2"], ["main", "feature"]);
            assert_eq!(view.heads["f1"], ["fix"]);
            assert_eq!(view.heads.len(), 2);
            assert!(!ids(view).contains(&"f1"), "fix is not merged");
        });
        let served = served.lock().unwrap();
        let mut others: Vec<&str> = served
            .listings()
            .into_iter()
            .filter(|listing| !listing.starts_with("branch=main&"))
            .collect();
        others.sort();
        assert_eq!(
            others,
            [
                "branch=empty&limit=1",
                "branch=feature&limit=1",
                "branch=fix&limit=1",
            ]
        );
    }

    /// A History tab opened before the branch list was read reads the other branches' heads
    /// once the list is in.
    #[gpui::test]
    async fn a_tab_opened_before_the_branch_list_reads_the_other_branches(cx: &mut TestAppContext) {
        let (session, served) = serve(
            Served {
                branches: vec![
                    ("main".into(), vec![commit("m1", 10, None, None)]),
                    ("fix".into(), vec![commit("m1", 10, None, None)]),
                ],
                ..Default::default()
            },
            cx,
        );
        let (view, cx) = open(&session, cx);
        let listings = served.lock().unwrap().listings().join(" ");
        assert!(listings.contains("branch=fix&limit=1"), "{listings}");
        view.read_with(cx, |view, _| assert_eq!(view.heads["m1"], ["main", "fix"]));
    }

    /// A commit to another branch moves that branch's tag without laying the history out again.
    #[gpui::test]
    async fn a_commit_to_another_branch_moves_its_tag_alone(cx: &mut TestAppContext) {
        // `main` merged `fix` at f1, and x1 from a branch not listed.
        let (session, served) = serve(
            Served {
                branches: vec![
                    (
                        "main".into(),
                        vec![
                            commit("m3", 60, Some("m2"), Some("x1")),
                            commit("m2", 30, Some("m1"), Some("f1")),
                            commit("m1", 10, None, None),
                        ],
                    ),
                    (
                        "fix".into(),
                        vec![
                            commit("f1", 20, Some("m1"), None),
                            commit("m1", 10, None, None),
                        ],
                    ),
                ],
                loose: vec![commit("x1", 50, Some("f1"), None)],
                ..Default::default()
            },
            cx,
        );
        let (view, cx) = open(&session, cx);
        // Studio watches `fix` (its branches panel, say), and so hears of commits to it.
        let _watch = session.update(cx, |session, cx| session.watch_head("fix", cx));
        cx.run_until_parked();
        let laid_out = view.read_with(cx, |view, _| {
            assert_eq!(ids(view), ["m3", "x1", "m2", "f1", "m1"]);
            assert_eq!(view.heads.get("f1"), Some(&vec!["fix".to_owned()]));
            view.commits.as_ptr()
        });
        served
            .lock()
            .unwrap()
            .chain_mut("fix")
            .insert(0, commit("f2", 70, Some("f1"), None));
        session
            .update(cx, |session, cx| session.head_now("fix", cx))
            .await;
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.commits.as_ptr(), laid_out, "laid out again");
            assert_eq!(view.heads.get("f2"), Some(&vec!["fix".to_owned()]));
            assert_eq!(view.heads.get("f1"), None);
        });
        let served = served.lock().unwrap();
        let fix: Vec<&str> = served
            .listings()
            .into_iter()
            .filter(|listing| listing.contains("branch=fix"))
            .collect();
        assert_eq!(fix, ["branch=fix&limit=1", "branch=fix&limit=1"]);
    }

    /// A session on a graph whose `main` holds one commit, `c1`, which changed nothing.
    fn one_commit_session(cx: &mut TestAppContext) -> Entity<GraphSession> {
        let served = Served {
            branches: vec![("main".into(), vec![commit("c1", 1, None, None)])],
            ..Default::default()
        };
        serve(served, cx).0
    }

    /// The commit shown beside the list draws on its own: a change there (a scroll, a page of
    /// changes read) leaves the list as it was drawn; a change of the history draws it again.
    #[gpui::test]
    async fn the_commit_beside_the_list_leaves_the_list_drawn(cx: &mut TestAppContext) {
        let session = one_commit_session(cx);
        let (view, cx) = open(&session, cx);
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
        let session = one_commit_session(cx);
        let (view, cx) = open(&session, cx);
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

    /// "New branch here" is offered at a published commit only. At one, the key starts a
    /// branch at the selected commit and the row's menu offers it; at a proposal or a snapshot
    /// the key is not available and the menu leaves it out.
    #[gpui::test]
    async fn new_branch_here_is_offered_only_at_a_published_commit(cx: &mut TestAppContext) {
        use studio_actions::graph::CreateBranchAt;

        let mut main = chain("m", 3);
        main[1]["kind"] = json!("unpublished");
        main[2]["kind"] = json!("unattached");
        let (session, _) = serve(
            Served {
                branches: vec![("main".into(), main)],
                ..Default::default()
            },
            cx,
        );
        let started = Arc::new(Mutex::new(Vec::new()));
        let heard = started.clone();
        cx.update(|cx| {
            cx.on_action(move |action: &CreateBranchAt, _| {
                heard.lock().unwrap().push(action.commit_id.clone())
            });
        });
        let (view, cx) = open(&session, cx);
        cx.update(|window, cx| window.focus(&view.focus_handle(cx), cx));
        let commits = view.read_with(cx, |view, _| view.commits.clone());
        assert_eq!(commits.len(), 3);
        for (ix, offered) in [(0, true), (1, false), (2, false)] {
            view.update(cx, |view, cx| view.select(ix, cx));
            cx.run_until_parked();
            let available = cx.update(|window, cx| window.is_action_available(&NewBranchHere, cx));
            assert_eq!(available, offered, "{}", commits[ix].graph_commit_id);
            cx.dispatch_action(NewBranchHere);
            cx.run_until_parked();
        }
        assert_eq!(*started.lock().unwrap(), ["m2"]);

        for (commit, offered) in commits.iter().zip([true, false, false]) {
            let (_, cx) = cx.add_window_view(|window, cx| {
                ContextMenu::new(window, cx, |menu, _, _| commit_menu(menu, &session, commit))
            });
            cx.run_until_parked();
            assert_eq!(
                cx.debug_bounds("MENU_ITEM-New branch here…").is_some(),
                offered,
                "{}",
                commit.graph_commit_id
            );
            assert!(cx.debug_bounds("MENU_ITEM-Open in a tab").is_some());
        }
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
            kind: graph_client::api::CommitKind::Published,
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
