//! Branches (spec 008): the sidebar list, switching, and the create and delete dialogs; and
//! the list as lanes of work (spec 043): grouped by the prefix of their names or by who wrote
//! them, each marked merged into main or with how many commits it is ahead, and a Refresh.
//!
//! A 0.14 server's `GET /branches` returns names only, so each branch's last commit comes from
//! its own `GET /commits?branch=` (a 0.15 server details every head in one read), for the
//! branches the list draws: none while it is hidden, and those in view while it is shown. A
//! drawn branch's lane (merged, or how far ahead) is its merge base with main and, when it is
//! not merged, one page of its history.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;

use editor::{Editor, EditorEvent};
use gpui::{
    AnyElement, App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render,
    SharedString, Subscription, Task, UniformListScrollHandle, WeakEntity, Window, uniform_list,
};
use graph_client::WriteOutcome;
use graph_client::api::{BranchStartRefusalOutput, BranchStartRefusalReason, CommitOutput};
use graph_store::{
    AccessAction, AccessScope, BranchStart, GraphSession, ServerFeature, SessionEvent,
};
use studio_actions::graph::{
    CompareBranches, CreateBranch, CreateBranchAt, DeleteBranch, OpenBranch, OpenBranchHistory,
    OpenCompare, RefreshBranches, ReviewBranch, ToggleBranchGrouping,
};
use studio_settings::{BranchGrouping, StudioSettings};
use studio_ui::ds::Tipped;
use studio_ui::menu::{ContextMenu, ContextMenuEntry};
use studio_ui::{ds, now_micros, relative_time, short_commit_id};
use studio_workspace::{Workspace, WorkspaceEvent};
use ui::prelude::*;
use ui::right_click_menu;

use crate::compare::{CompareModal, open_compare};
use crate::view::{open_review, open_review_with};
use studio_workspace::ModalView;

/// The branch every review and merge targets.
pub const TARGET: &str = "main";

#[derive(Debug, Clone, PartialEq)]
struct BranchDetail {
    head: String,
    created_at: i64,
    actor: Option<String>,
    /// When the branch was made, and what from, where the server details its branches
    /// (Omnigraph 0.15, `graph_store::ServerFeature::DetailedBranchList`); `None` else.
    started: Option<(i64, Option<String>)>,
}

/// How many of a branch's commits its lane reads to count those ahead of main: one page.
pub(crate) const AHEAD_PAGE: u32 = 100;

/// Where a branch stands against [`TARGET`], as its row marks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lane {
    /// Its head is in main: deleting it loses nothing.
    Merged,
    /// It holds commits main does not: how many, or that there are more than a page counts.
    Ahead { commits: usize, more: bool },
    /// The merge base was read, the history was not: not merged, by how much is not known.
    Unmerged,
}

/// How many of `commits` (a branch's history, newest first) are not in main: those down to the
/// merge base `base`, or to the first commit written on main, which main still holds. Past the
/// page, there may be more.
pub(crate) fn ahead_of(commits: &[CommitOutput], base: &str, page_ended: bool) -> Lane {
    let stop = commits
        .iter()
        .position(|commit| commit.graph_commit_id == base || commit.graph_branch.is_none());
    match stop {
        Some(commits) => Lane::Ahead {
            commits,
            more: false,
        },
        None => Lane::Ahead {
            commits: commits.len(),
            more: !page_ended,
        },
    }
}

/// One row of the list: a branch, a group's header, or "New branch…" after the last.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ListRow {
    /// The branch at `index` of the session's list, named from byte `shown_from` on (a grouped
    /// branch drops its group's prefix), and indented under its group's header when grouped.
    Branch {
        index: usize,
        shown_from: usize,
        grouped: bool,
    },
    Group(usize),
    NewBranch,
}

/// A group of branches: the prefix their names share, or who wrote their newest commits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BranchGroup {
    /// What the list keeps folded by: the grouping and the prefix or the author.
    pub key: String,
    pub label: String,
    pub branches: usize,
    pub open: bool,
}

/// The list's rows, made again only when the branches, the grouping, a fold or the authors
/// change: a draw reads the rows in view from it.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Layout {
    pub rows: Vec<ListRow>,
    pub groups: Vec<BranchGroup>,
}

/// The part of `branch` before its first `/`, when something follows it.
fn prefix_of(branch: &str) -> Option<&str> {
    branch
        .split_once('/')
        .filter(|(prefix, rest)| !prefix.is_empty() && !rest.is_empty())
        .map(|(prefix, _)| prefix)
}

/// Lays the branches out (spec 043): [`TARGET`] first, then, by prefix, the branches whose
/// prefix no other branch shares, then a group for each shared prefix, its branches named
/// without it; by author, a group for each author, in order of their names, the branches whose
/// author is not known last. A folded group keeps its header. "New branch…" is the last row.
pub(crate) fn lay_out(
    branches: &[String],
    grouping: BranchGrouping,
    author: impl Fn(&str) -> Option<String>,
    folded: &HashSet<String>,
) -> Layout {
    let mut layout = Layout::default();
    let mut rest: Vec<usize> = Vec::new();
    for (index, branch) in branches.iter().enumerate() {
        if branch == TARGET {
            layout.rows.push(ListRow::Branch {
                index,
                shown_from: 0,
                grouped: false,
            });
        } else {
            rest.push(index);
        }
    }
    // Each group's key, its label and its branches, in the list's order.
    let mut groups: Vec<(String, String, Vec<usize>)> = Vec::new();
    match grouping {
        BranchGrouping::Prefix => {
            let mut shared: HashMap<&str, usize> = HashMap::new();
            for &index in &rest {
                if let Some(prefix) = prefix_of(&branches[index]) {
                    *shared.entry(prefix).or_default() += 1;
                }
            }
            for &index in &rest {
                match prefix_of(&branches[index]).filter(|prefix| shared[prefix] > 1) {
                    Some(prefix) => {
                        let key = format!("prefix:{prefix}");
                        match groups.iter_mut().find(|(k, ..)| *k == key) {
                            Some((.., members)) => members.push(index),
                            None => groups.push((key, format!("{prefix}/"), vec![index])),
                        }
                    }
                    None => layout.rows.push(ListRow::Branch {
                        index,
                        shown_from: 0,
                        grouped: false,
                    }),
                }
            }
            groups.sort_by(|a, b| a.1.cmp(&b.1));
        }
        BranchGrouping::Author => {
            for &index in &rest {
                let author = author(&branches[index]);
                let key = format!("author:{}", author.as_deref().unwrap_or_default());
                match groups.iter_mut().find(|(k, ..)| *k == key) {
                    Some((.., members)) => members.push(index),
                    None => groups.push((
                        key,
                        author.unwrap_or_else(|| UNKNOWN_AUTHOR.to_owned()),
                        vec![index],
                    )),
                }
            }
            // By name, the branches nobody is known to have written last.
            groups.sort_by(|a, b| {
                let unknown = |key: &str| key == "author:";
                unknown(&a.0).cmp(&unknown(&b.0)).then(a.1.cmp(&b.1))
            });
        }
    }
    for (key, label, members) in groups {
        let open = !folded.contains(&key);
        let shown_from = match grouping {
            BranchGrouping::Prefix => label.len(),
            BranchGrouping::Author => 0,
        };
        layout.rows.push(ListRow::Group(layout.groups.len()));
        layout.groups.push(BranchGroup {
            key,
            label,
            branches: members.len(),
            open,
        });
        if open {
            layout
                .rows
                .extend(members.into_iter().map(|index| ListRow::Branch {
                    index,
                    shown_from,
                    grouped: true,
                }));
        }
    }
    layout.rows.push(ListRow::NewBranch);
    layout
}

/// The group of branches whose newest commit's author is not known: the server did not detail
/// them (one the credential may not read).
const UNKNOWN_AUTHOR: &str = "Author not known";

/// The "Branches" sidebar section.
pub struct BranchList {
    workspace: WeakEntity<Workspace>,
    session: Option<Entity<GraphSession>>,
    details: HashMap<String, BranchDetail>,
    /// Each drawn branch's lane, kept while it is read again: a row's mark changes only when
    /// the new answer lands.
    lanes: HashMap<String, Lane>,
    /// The branches whose lane has been asked for since it last could have changed: a commit
    /// to the branch or to main asks again for those drawn.
    lanes_asked: HashSet<String>,
    lane_reads: HashMap<String, Task<()>>,
    unread_lanes: Vec<String>,
    /// The rows, made again when the branches, the grouping, a fold or the authors change.
    layout: Option<Rc<Layout>>,
    /// The groups folded to their header, by [`BranchGroup::key`].
    folded: HashSet<String>,
    /// The grouping the layout was made with, as the settings last said.
    grouping: BranchGrouping,
    /// The detailed branch list a grouping by author waits for.
    _authors_read: Option<Task<()>>,
    /// The branches whose head has been asked for since it last could have moved: read, or
    /// being read. A commit to one, or a change of the list, asks again for those drawn.
    asked: HashSet<String>,
    /// Each branch's head read, by branch: a newer one takes the place of one still out.
    reads: HashMap<String, Task<()>>,
    /// Branches drawn without their head asked for, and whether a read of them is due once the
    /// frame that drew them is out ([`Self::read_drawn`]).
    unread: Vec<String>,
    unread_due: bool,
    /// The list's scroll: it draws only the rows in view.
    scroll: UniformListScrollHandle,
    /// How many rows the list has built, for the test that it builds only those in view.
    #[cfg(test)]
    rows_drawn: usize,
    /// How many times a lane landing asked the list to draw, for the test that lanes read again
    /// to the same answers draw nothing.
    #[cfg(test)]
    lane_draws: usize,
    _workspace_subscription: Option<Subscription>,
    _session_subscription: Option<Subscription>,
    _settings_subscription: Option<Subscription>,
}

impl BranchList {
    /// `session` is the workspace's session when the list is created; later sessions arrive
    /// through [`WorkspaceEvent::SessionChanged`].
    pub fn new(
        workspace: WeakEntity<Workspace>,
        session: Option<Entity<GraphSession>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let grouping = cx
            .try_global::<StudioSettings>()
            .map_or_else(BranchGrouping::default, |settings| {
                settings.studio.branch_grouping
            });
        let settings_subscription = cx.try_global::<StudioSettings>().is_some().then(|| {
            cx.observe_global::<StudioSettings>(|this: &mut Self, cx| {
                let grouping = StudioSettings::get(cx).studio.branch_grouping;
                if grouping != this.grouping {
                    this.grouping = grouping;
                    this.relayout(cx);
                }
            })
        });
        let mut this = Self {
            workspace: workspace.clone(),
            session: None,
            details: HashMap::new(),
            lanes: HashMap::new(),
            lanes_asked: HashSet::new(),
            lane_reads: HashMap::new(),
            unread_lanes: Vec::new(),
            layout: None,
            folded: HashSet::new(),
            grouping,
            _authors_read: None,
            asked: HashSet::new(),
            reads: HashMap::new(),
            unread: Vec::new(),
            unread_due: false,
            scroll: UniformListScrollHandle::new(),
            #[cfg(test)]
            rows_drawn: 0,
            #[cfg(test)]
            lane_draws: 0,
            _workspace_subscription: None,
            _session_subscription: None,
            _settings_subscription: settings_subscription,
        };
        if let Some(workspace) = workspace.upgrade() {
            this._workspace_subscription = Some(cx.subscribe_in(
                &workspace,
                window,
                |this, workspace, event: &WorkspaceEvent, _, cx| match event {
                    WorkspaceEvent::SessionChanged => {
                        this.watch(workspace.read(cx).session().cloned(), cx)
                    }
                    WorkspaceEvent::ActiveItemChanged => cx.notify(),
                },
            ));
        }
        this.watch(session, cx);
        this
    }

    fn watch(&mut self, session: Option<Entity<GraphSession>>, cx: &mut Context<Self>) {
        // The same graph shown again (another opened behind it): its details hold.
        if self.session.as_ref().map(Entity::entity_id) == session.as_ref().map(Entity::entity_id) {
            return;
        }
        self._session_subscription = session.as_ref().map(|session| {
            cx.subscribe(
                session,
                |this, session, event: &SessionEvent, cx| match event {
                    SessionEvent::BranchesChanged => this.branches_changed(&session, cx),
                    // The session forgot the branch's head: a read still out answers for before.
                    // Its lane is asked for again; a commit to main may have merged any branch,
                    // or moved its merge base, so every drawn lane is.
                    SessionEvent::Committed { branch } => {
                        this.asked.remove(branch);
                        this.reads.remove(branch);
                        this.lane_moved(branch);
                        cx.notify();
                    }
                    SessionEvent::HeadMoved { branch, .. } if branch == TARGET => {
                        this.lane_moved(branch);
                        cx.notify();
                    }
                    // A server that details its branches now, or no longer, can group by author.
                    SessionEvent::BranchChanged | SessionEvent::CredentialChanged => {
                        this.relayout(cx)
                    }
                    _ => {}
                },
            )
        });
        self.session = session;
        // The details shown and the reads still out are the previous graph's: a read landing
        // after the switch would name its heads on this graph's rows. This graph's are read
        // once its branches are drawn.
        self.details.clear();
        self.asked.clear();
        self.reads.clear();
        self.unread.clear();
        self.lanes.clear();
        self.lanes_asked.clear();
        self.lane_reads.clear();
        self.unread_lanes.clear();
        self.folded.clear();
        self._authors_read = None;
        self.relayout(cx);
    }

    /// `branch` moved: its lane is asked for again when next drawn, and every lane when it is
    /// main. The marks shown stay until the answers land.
    fn lane_moved(&mut self, branch: &str) {
        if branch == TARGET {
            self.lanes_asked.clear();
            self.lane_reads.clear();
        } else {
            self.lanes_asked.remove(branch);
            self.lane_reads.remove(branch);
        }
    }

    /// The grouping the list draws: by author only where the server details every branch's
    /// head (`ServerFeature::DetailedBranchList`), so the groups are known in one read; else
    /// by prefix.
    fn grouping_shown(&self, cx: &App) -> BranchGrouping {
        match (self.grouping, &self.session) {
            (BranchGrouping::Author, Some(session))
                if session
                    .read(cx)
                    .features(cx)
                    .has(ServerFeature::DetailedBranchList) =>
            {
                BranchGrouping::Author
            }
            _ => BranchGrouping::Prefix,
        }
    }

    /// The rows are made again at the next draw: the branches, the grouping, a fold or the
    /// authors changed. A grouping by author whose authors are not read yet reads them first.
    fn relayout(&mut self, cx: &mut Context<Self>) {
        self.layout = None;
        if self.grouping_shown(cx) == BranchGrouping::Author {
            self.read_authors(cx);
        }
        cx.notify();
    }

    /// Reads every branch's head in one detailed list, for a grouping by author, unless the
    /// session holds that list's answer for the branches it lists.
    fn read_authors(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else {
            return;
        };
        let Some(read) = session.update(cx, |session, cx| session.read_branch_details(cx)) else {
            return;
        };
        self._authors_read = Some(cx.spawn(async move |this, cx| {
            read.await;
            this.update(cx, |this, cx| {
                this._authors_read = None;
                this.layout = None;
                cx.notify();
            })
            .ok();
        }));
    }

    /// The layout of the branches the session lists, made now if it is not held.
    fn layout(&mut self, cx: &App) -> Option<Rc<Layout>> {
        if let Some(layout) = &self.layout {
            return Some(layout.clone());
        }
        let session = self.session.as_ref()?.read(cx);
        let branches = session.branches.loaded()?;
        let grouping = self.grouping_shown(cx);
        let details = &self.details;
        let layout = Rc::new(lay_out(
            branches,
            grouping,
            |branch| {
                session
                    .branch_detail(branch)
                    .map(|detail| detail.head.actor_id.clone())
                    .or_else(|| details.get(branch).map(|detail| detail.actor.clone()))
                    .flatten()
            },
            &self.folded,
        ));
        self.layout = Some(layout.clone());
        Some(layout)
    }

    fn toggle_group(&mut self, key: String, cx: &mut Context<Self>) {
        if !self.folded.remove(&key) {
            self.folded.insert(key);
        }
        self.layout = None;
        cx.notify();
    }

    /// The branch list changed. A branch made, or deleted and made again, has another head, so
    /// those drawn are asked for again, the session answering the others from what it holds;
    /// one no longer listed is forgotten with its read.
    fn branches_changed(&mut self, session: &Entity<GraphSession>, cx: &mut Context<Self>) {
        let listed = session.read(cx).branches.loaded();
        let listed = |branch: &String| listed.is_some_and(|list| list.contains(branch));
        self.details.retain(|branch, _| listed(branch));
        self.reads.retain(|branch, _| listed(branch));
        self.asked.clear();
        self.lanes.retain(|branch, _| listed(branch));
        self.lane_reads.retain(|branch, _| listed(branch));
        self.lanes_asked.clear();
        self.relayout(cx);
    }

    /// Notes the branches of `drawn` whose head is still to be asked for, to read once the
    /// frame is out: a list hidden in its dock is not drawn, so it asks for none, and a shown
    /// one only for the rows in view. Each is asked for once until it could have moved.
    fn note_drawn(&mut self, drawn: &[String], window: &mut Window, cx: &mut Context<Self>) {
        for branch in drawn {
            if !self.asked.contains(branch) && !self.unread.contains(branch) {
                self.unread.push(branch.clone());
            }
            if branch != TARGET
                && !self.lanes_asked.contains(branch)
                && !self.unread_lanes.contains(branch)
            {
                self.unread_lanes.push(branch.clone());
            }
        }
        let due = !self.unread.is_empty() || !self.unread_lanes.is_empty();
        if due && !std::mem::replace(&mut self.unread_due, true) {
            cx.defer_in(window, |this, _, cx| {
                this.unread_due = false;
                this.read_drawn(cx);
            });
        }
    }

    /// Each drawn branch's head commit, read alone (or from a commit list the session holds:
    /// after a commit, only the branch it landed on is read again).
    fn read_drawn(&mut self, cx: &mut Context<Self>) {
        let unread = std::mem::take(&mut self.unread);
        let Some(session) = self.session.clone() else {
            return;
        };
        for branch in std::mem::take(&mut self.unread_lanes) {
            if self.lanes_asked.insert(branch.clone()) {
                self.read_lane(&session, branch, cx);
            }
        }
        for branch in unread {
            if !self.asked.insert(branch.clone()) {
                continue;
            }
            let read = session.update(cx, |session, cx| session.head_commit(&branch, false, cx));
            let key = branch.clone();
            let task = cx.spawn(async move |this, cx| {
                let head = read.await;
                this.update(cx, |this, cx| {
                    let started = this.session.as_ref().and_then(|session| {
                        let detail = session.read(cx).branch_detail(&branch)?;
                        Some((detail.created_at?, detail.from.clone()))
                    });
                    let changed = match head {
                        Ok(Some(head)) => {
                            let detail = BranchDetail {
                                head: head.graph_commit_id,
                                created_at: head.created_at,
                                actor: head.actor_id,
                                started,
                            };
                            this.details.insert(branch, detail.clone()) != Some(detail)
                        }
                        _ => this.details.remove(&branch).is_some(),
                    };
                    // A head read again to the same commit leaves the list as it was drawn.
                    if changed {
                        // Grouped by author, the groups may have changed with it.
                        if this.grouping_shown(cx) == BranchGrouping::Author {
                            this.layout = None;
                        }
                        cx.notify();
                    }
                })
                .ok();
            });
            self.reads.insert(key, task);
        }
    }

    /// Reads `branch`'s lane: its merge base with main, then, when its head is not in main, a
    /// page of its history to count the commits main does not hold. A read that fails leaves
    /// the row unmarked.
    fn read_lane(
        &mut self,
        session: &Entity<GraphSession>,
        branch: String,
        cx: &mut Context<Self>,
    ) {
        let base = session.update(cx, |session, cx| session.merge_base(&branch, TARGET, cx));
        let key = branch.clone();
        let task = cx.spawn(async move |this, cx| {
            let lane = match base.await {
                Ok(base) if base.merge_base.graph_commit_id == base.source_commit_id => {
                    Some(Lane::Merged)
                }
                Ok(base) => {
                    let page = this
                        .update(cx, |this, cx| {
                            this.session.as_ref().map(|session| {
                                session.update(cx, |session, cx| {
                                    session.commit_page(&branch, AHEAD_PAGE, None, cx)
                                })
                            })
                        })
                        .ok()
                        .flatten();
                    match page {
                        Some(page) => Some(match page.await {
                            Ok(list) => ahead_of(
                                &list.commits,
                                &base.merge_base.graph_commit_id,
                                list.next_page_token.is_none(),
                            ),
                            Err(_) => Lane::Unmerged,
                        }),
                        None => return,
                    }
                }
                Err(_) => None,
            };
            this.update(cx, |this, cx| {
                this.lane_reads.remove(&branch);
                let shown = match lane {
                    Some(lane) => this.lanes.insert(branch.clone(), lane) != Some(lane),
                    None => this.lanes.remove(&branch).is_some(),
                };
                // A lane read again to the same answer leaves the list as it was drawn.
                if shown {
                    #[cfg(test)]
                    {
                        this.lane_draws += 1;
                    }
                    cx.notify();
                }
            })
            .ok();
        });
        self.lane_reads.insert(key, task);
    }

    /// The end of a branch's row for its lane, and what its tooltip adds.
    fn lane_words(lane: Option<Lane>) -> (Option<String>, Option<String>) {
        match lane {
            Some(Lane::Merged) => (None, Some(format!("merged into {TARGET}"))),
            Some(Lane::Ahead { commits: 0, .. }) | Some(Lane::Unmerged) | None => (None, None),
            Some(Lane::Ahead { commits, more }) => {
                let count = if more {
                    format!("{commits}+")
                } else {
                    commits.to_string()
                };
                (
                    Some(format!("{count} ahead")),
                    Some(format!(
                        "{} not in {TARGET}",
                        if more {
                            format!("{commits} or more commits")
                        } else {
                            studio_ui::plural(commits, "commit", "commits")
                        }
                    )),
                )
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render_branch(
        &self,
        index: usize,
        branch: String,
        shown_from: usize,
        grouped: bool,
        current: bool,
        lit: bool,
    ) -> impl IntoElement + use<> {
        let lane = self.lanes.get(&branch).copied();
        let merged = lane == Some(Lane::Merged);
        let (ahead, lane_tip) = Self::lane_words(lane);
        let detail = self.details.get(&branch).map(|d| {
            let actor = d.actor.clone().unwrap_or_else(|| "unknown actor".into());
            let now = now_micros();
            let when = relative_time(d.created_at, now);
            let started = match &d.started {
                Some((at, Some(from))) => {
                    format!(" · made {} from {from}", relative_time(*at, now))
                }
                Some((at, None)) => format!(" · made {}", relative_time(*at, now)),
                None => String::new(),
            };
            (
                when.clone(),
                format!(
                    "Head {} · {actor} · {when}{started}",
                    short_commit_id(&d.head)
                ),
            )
        });
        // A grouped branch's name is said whole in its tooltip, first.
        let tooltip = {
            let mut lines: Vec<String> = Vec::new();
            if shown_from > 0 {
                lines.push(branch.clone());
            }
            let mut facts: Vec<String> = detail.iter().map(|(_, facts)| facts.clone()).collect();
            facts.extend(lane_tip);
            if !facts.is_empty() {
                lines.push(facts.join(" · "));
            }
            (!lines.is_empty()).then(|| lines.join("\n"))
        };
        let shown_name: SharedString = branch
            .get(shown_from..)
            .filter(|rest| !rest.is_empty())
            .unwrap_or(&branch)
            .to_owned()
            .into();
        let session = self.session.clone();
        let workspace = self.workspace.clone();
        let name = branch.clone();
        let is_target = branch == TARGET;
        right_click_menu(("branch-menu", index))
            .trigger({
                let session = session.clone();
                let workspace = workspace.clone();
                move |_, _, cx| {
                    let when = detail.clone().map(|(when, _)| when);
                    // A merged branch says so in place of its time; one with commits of its own
                    // says how many in its place.
                    let delete_refused = session.as_ref().and_then(|session| {
                        session
                            .read(cx)
                            .refusal(AccessAction::BranchDelete, &AccessScope::branch(&name))
                    });
                    // Opening a branch leaves the window on the one it reads; moving the window
                    // is this button, the top bar's popover or the row's menu (spec 020).
                    let end = h_flex()
                        .gap(px(studio_ui::ds::space::XS))
                        .when(merged, |end| end.child(studio_ui::ds::tag("merged", cx)))
                        // How far ahead takes the time's place, which the tooltip still says:
                        // the sidebar has room for one fact beside a branch's name.
                        .children(ahead.clone().map(studio_ui::ds::sidebar_count))
                        .when(!merged && ahead.is_none(), |end| {
                            end.children(when.map(studio_ui::ds::sidebar_count))
                        })
                        .map(|end| {
                            if current {
                                end.child(studio_ui::ds::text::meta("current"))
                            } else if merged {
                                // Merged, nothing is lost with it: Delete asks with ↵.
                                let session = session.clone();
                                let workspace = workspace.clone();
                                let name = name.clone();
                                let refused = delete_refused.clone();
                                end.child(
                                    studio_ui::ds::row_button(("delete", index), "Delete")
                                        .disabled(refused.is_some())
                                        .tip(studio_ui::ds::tooltip(refused.unwrap_or_else(|| {
                                            format!(
                                                "Everything on it is in {TARGET}: \
                                                     deleting it loses nothing"
                                            )
                                        })))
                                        .on_click(move |_, window, cx| {
                                            cx.stop_propagation();
                                            if let Some(session) = session.clone() {
                                                open_delete(&workspace, session, &name, window, cx);
                                            }
                                        }),
                                )
                            } else {
                                let session = session.clone();
                                let name = name.clone();
                                end.child(
                                    studio_ui::ds::row_button(("switch", index), "Switch")
                                        .tip(studio_ui::ds::tooltip("Read and edit this branch"))
                                        .on_click(move |_, _, cx| {
                                            cx.stop_propagation();
                                            if let Some(session) = &session {
                                                session.update(cx, |s, cx| {
                                                    s.set_branch(name.clone(), cx)
                                                });
                                            }
                                        }),
                                )
                            }
                        });
                    let row = studio_ui::ds::SidebarRow::new(("branch", index), shown_name.clone())
                        .icon(IconName::GitBranch)
                        .selected(lit)
                        .muted(merged)
                        .end(end);
                    let row = match tooltip.clone() {
                        Some(tooltip) => row.tip(studio_ui::ds::tooltip(tooltip)),
                        None => row,
                    };
                    let row = row.on_click({
                        let name = name.clone();
                        move |event, window, cx| {
                            window.dispatch_action(
                                Box::new(OpenBranch {
                                    branch: name.clone(),
                                    preview: event.click_count() < 2,
                                }),
                                cx,
                            )
                        }
                    });
                    // Under its group's header, a branch is inset by the header's chevron.
                    div()
                        .when(grouped, |row| row.pl(px(ds::space::LG)))
                        .child(row)
                }
            })
            .menu(move |window, cx| {
                let session = session.clone();
                let workspace = workspace.clone();
                let name = branch.clone();
                // What the credential may do with the branch is read as its menu opens (once,
                // until a 403 or a new token); until the answer lands, Delete is offered and the
                // server decides. A merge is judged with its target, which the review reads.
                let (delete_refused, merge_refused) = match &session {
                    Some(session) => session.update(cx, |session, cx| {
                        let own = AccessScope::branch(&name);
                        session.read_access(own.clone(), cx);
                        (
                            session.refusal(AccessAction::BranchDelete, &own),
                            session.refusal(
                                AccessAction::BranchMerge,
                                &AccessScope::branch_into(&name, TARGET),
                            ),
                        )
                    }),
                    None => (None, None),
                };
                ContextMenu::build(window, cx, move |menu, _, _| {
                    let switch = {
                        let session = session.clone();
                        let name = name.clone();
                        move |_: &mut Window, cx: &mut App| {
                            if let Some(session) = &session {
                                session.update(cx, |s, cx| s.set_branch(name.clone(), cx));
                            }
                        }
                    };
                    let review = {
                        let workspace = workspace.clone();
                        let name = name.clone();
                        move |window: &mut Window, cx: &mut App| {
                            workspace
                                .update(cx, |w, cx| {
                                    open_review(w, name.clone(), TARGET.into(), window, cx)
                                })
                                .ok();
                        }
                    };
                    // A merge is made from the review, which holds it.
                    let merge = review.clone();
                    let delete = {
                        let workspace = workspace.clone();
                        let session = session.clone();
                        let name = name.clone();
                        move |window: &mut Window, cx: &mut App| {
                            let Some(session) = session.clone() else {
                                return;
                            };
                            workspace
                                .update(cx, |w, cx| {
                                    w.toggle_modal(window, cx, |window, cx| {
                                        DeleteBranchModal::new(session, name.clone(), window, cx)
                                    })
                                })
                                .ok();
                        }
                    };
                    // Compared with another branch or a commit, read-only (spec 043).
                    let compare = {
                        let workspace = workspace.clone();
                        let session = session.clone();
                        let name = name.clone();
                        move |window: &mut Window, cx: &mut App| {
                            if let Some(session) = session.clone() {
                                open_compare_dialog(&workspace, session, name.clone(), window, cx);
                            }
                        }
                    };
                    menu.header(name.clone())
                        .entry("Switch to this branch", None, switch)
                        .item(
                            ContextMenuEntry::new(format!("Review against {TARGET}"))
                                .disabled(is_target)
                                .handler(review),
                        )
                        .item(ContextMenuEntry::new("Compare with…").handler(compare))
                        .item(
                            ContextMenuEntry::new(match &merge_refused {
                                Some(reason) if !is_target => {
                                    format!("Merge into {TARGET} ({})", aside(reason))
                                }
                                _ => format!("Merge into {TARGET}…"),
                            })
                            .disabled(is_target || merge_refused.is_some())
                            .handler(merge),
                        )
                        .separator()
                        .item(
                            // A menu entry has no tooltip: a disabled one says why in its label.
                            ContextMenuEntry::new(match &delete_refused {
                                _ if is_target => format!("Delete ({TARGET} cannot be deleted)"),
                                Some(reason) => format!("Delete ({})", aside(reason)),
                                None => "Delete…".to_owned(),
                            })
                            .icon(IconName::Trash)
                            .disabled(is_target || delete_refused.is_some())
                            .handler(delete),
                        )
                })
            })
    }
}

impl BranchList {
    /// The "New branch…" row, after the branches.
    fn render_new_branch(current: &str) -> studio_ui::ds::SidebarRow {
        let current = current.to_owned();
        studio_ui::ds::SidebarRow::new("new-branch", "New branch…")
            .muted(true)
            .icon(IconName::Plus)
            .tip_fn(move |_, cx| {
                studio_ui::ds::tooltip_with_meta(
                    format!("New branch from {current}"),
                    Some(&CreateBranch),
                    format!("omnigraph branch create <name> --from {current}"),
                    cx,
                )
            })
            .on_click(|_, window, cx| window.dispatch_action(Box::new(CreateBranch), cx))
    }

    /// The header of a group of branches: a chevron that folds it, its prefix or author, and
    /// how many branches it holds.
    fn render_group(
        &self,
        index: usize,
        group: &BranchGroup,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = group.key.clone();
        let tip = match group.key.split_once(':') {
            Some(("author", "")) => {
                "Branches the server does not say who wrote: one the credential may not read"
                    .to_owned()
            }
            Some(("author", author)) => format!("Branches whose newest commit {author} wrote"),
            _ => format!("Branches named {}…", group.label),
        };
        let row = studio_ui::ds::SidebarRow::new(("branch-group", index), group.label.clone())
            .start(studio_ui::ds::icon::meta(if group.open {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            }))
            .muted(true)
            .end(studio_ui::ds::sidebar_count(group.branches.to_string()))
            .tip(studio_ui::ds::tooltip(format!(
                "{tip}. Click to {} them.",
                if group.open { "fold" } else { "show" }
            )))
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_group(key.clone(), cx)));
        // As wide as the branch rows, so its count lines up with theirs.
        div().w_full().child(row).into_any_element()
    }

    /// The rows in `range` of the list: the branches and group headers it reaches, each branch
    /// with its right-click menu (built only when it opens), then "New branch…" after the last.
    /// The heads and lanes of the branches drawn are read after the frame
    /// ([`Self::note_drawn`]).
    fn render_rows(
        &mut self,
        range: Range<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        #[cfg(test)]
        {
            self.rows_drawn += range.len();
        }
        let Some(session) = self.session.clone() else {
            return Vec::new();
        };
        let Some(layout) = self.layout(cx) else {
            return Vec::new();
        };
        let current = session.read(cx).branch.clone();
        // The branch the active tab shows is the lit row; the one the window reads says so.
        let active = self
            .workspace
            .upgrade()
            .and_then(|workspace| workspace.read(cx).active_item()?.thing(cx))
            .and_then(|thing| match thing {
                studio_actions::Thing::Branch(branch) => Some(branch),
                _ => None,
            });
        let mut drawn = Vec::new();
        let mut rows = Vec::with_capacity(range.len());
        for at in range {
            let Some(row) = layout.rows.get(at) else {
                continue;
            };
            let element = match row {
                ListRow::Branch {
                    index,
                    shown_from,
                    grouped,
                } => {
                    let Some(branch) = session
                        .read(cx)
                        .branches
                        .loaded()
                        .and_then(|branches| branches.get(*index))
                        .cloned()
                    else {
                        continue;
                    };
                    drawn.push(branch.clone());
                    let is_current = branch == current;
                    let lit = active.as_deref() == Some(branch.as_str());
                    self.render_branch(*index, branch, *shown_from, *grouped, is_current, lit)
                        .into_any_element()
                }
                ListRow::Group(group) => match layout.groups.get(*group) {
                    Some(info) => self.render_group(*group, info, cx),
                    None => continue,
                },
                ListRow::NewBranch => Self::render_new_branch(&current).into_any_element(),
            };
            rows.push(element);
        }
        self.note_drawn(&drawn, window, cx);
        rows
    }
}

impl Render for BranchList {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(session) = self.session.clone() else {
            return v_flex().into_any_element();
        };
        // Grouped by author, the rows wait for the detailed list that names every author, so no
        // branch moves to its group as its head lands.
        let authors_pending = self._authors_read.is_some();
        let rows = if authors_pending {
            None
        } else {
            self.layout(cx).map(|layout| layout.rows.len())
        };
        let s = session.read(cx);
        let failed = match &s.branches {
            graph_store::Loadable::Failed(error) => Some(error.clone()),
            _ => None,
        };
        let current = s.branch.clone();
        // Every branch is a row of a list that draws only the rows in view, as a graph can have
        // thousands; "New branch…" is the row after the last.
        if let Some(rows) = rows {
            return uniform_list(
                "branch-rows",
                rows,
                cx.processor(|this, range: Range<usize>, window, cx| {
                    this.render_rows(range, window, cx)
                }),
            )
            .track_scroll(&self.scroll)
            .size_full()
            .pb(px(ds::space::MD))
            .into_any_element();
        }
        let note = match failed {
            Some(error) => div()
                .px(px(ds::space::MD))
                .child(studio_ui::ds::text::error_note(format!(
                    "Branches could not be read: {error}"
                ))),
            None if authors_pending && s.branches.loaded().is_some() => {
                studio_ui::ds::sidebar_note("Reading who wrote each branch…")
            }
            None => studio_ui::ds::sidebar_note("Reading branches…"),
        };
        v_flex()
            .w_full()
            .child(note)
            .child(Self::render_new_branch(&current))
            .into_any_element()
    }
}

/// A refusal as a disabled menu entry's aside: "Delete (your credential may not delete fix)".
fn aside(reason: &str) -> String {
    let mut chars = reason.chars();
    chars
        .next()
        .map(|first| first.to_lowercase().chain(chars).collect())
        .unwrap_or_default()
}

// ---- dialogs ----

fn field(editor: &Entity<Editor>, cx: &App) -> Div {
    studio_ui::ds::field_frame(false, cx).child(studio_ui::ds::field_input(editor.clone()))
}

/// Where the new branch starts, as the dialog shows it.
enum Start {
    /// The current branch's head.
    Head(String),
    /// A published commit, read as the dialog opens to say who made it and when.
    Commit { id: String, read: CommitRead },
}

enum CommitRead {
    Reading,
    Read(CommitOutput),
    /// Not read (a token whose read grant names branches is refused commits by id): the commit
    /// is named by its id alone, and the server still decides.
    Failed,
}

/// "New branch": a name, then the branch is created where it starts and switched to: the
/// current branch's head, or a published commit (History's "New branch here").
pub struct CreateBranchModal {
    session: Entity<GraphSession>,
    start: Start,
    name: Entity<Editor>,
    error: Option<SharedString>,
    /// Why the server would not start a branch at the commit. It is the commit's, not the
    /// name's, so it stays while the name changes, and Create with it: another name would be
    /// refused the same.
    refused_start: Option<SharedString>,
    running: bool,
    focus_handle: FocusHandle,
    _read: Option<Task<()>>,
    _task: Option<Task<()>>,
    _subscription: Subscription,
}

impl EventEmitter<DismissEvent> for CreateBranchModal {}

impl ModalView for CreateBranchModal {
    /// A write still running keeps the dialog open: its outcome shows here.
    fn on_before_dismiss(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> bool {
        !self.running
    }
}

impl Focusable for CreateBranchModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name.focus_handle(cx)
    }
}

impl CreateBranchModal {
    /// At the current branch's head.
    pub fn new(session: Entity<GraphSession>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let from = session.read(cx).branch.clone();
        Self::starting(session, Start::Head(from), window, cx)
    }

    /// At a published commit, which is read for who made it and when.
    pub fn at_commit(
        session: Entity<GraphSession>,
        commit_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let read = session.update(cx, |session, cx| session.commit(&commit_id, cx));
        let start = Start::Commit {
            id: commit_id,
            read: CommitRead::Reading,
        };
        let mut this = Self::starting(session, start, window, cx);
        this._read = Some(cx.spawn(async move |this, cx| {
            let read = read.await;
            this.update(cx, |this, cx| {
                if let Start::Commit { read: shown, .. } = &mut this.start {
                    *shown = read.map_or(CommitRead::Failed, CommitRead::Read);
                }
                cx.notify();
            })
            .ok();
        }));
        this
    }

    fn starting(
        session: Entity<GraphSession>,
        start: Start,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Branch name, for example fix-titles", window, cx);
            editor
        });
        let subscription = cx.subscribe(&name, |this, _, event: &EditorEvent, cx| {
            if let EditorEvent::BufferEdited = event {
                this.error = None;
                cx.notify();
            }
        });
        Self {
            session,
            start,
            name,
            error: None,
            refused_start: None,
            running: false,
            focus_handle: cx.focus_handle(),
            _read: None,
            _task: None,
            _subscription: subscription,
        }
    }

    fn branch_start(&self) -> BranchStart {
        match &self.start {
            Start::Head(branch) => BranchStart::Head(branch.clone()),
            Start::Commit { id, .. } => BranchStart::Commit(id.clone()),
        }
    }

    fn create(&mut self, cx: &mut Context<Self>) {
        if self.running || self.refused_start.is_some() {
            return;
        }
        let name = self.name.read(cx).text(cx).trim().to_owned();
        if name.is_empty() {
            self.error = Some("Enter a name for the branch.".into());
            cx.notify();
            return;
        }
        self.running = true;
        let start = self.branch_start();
        let task = self
            .session
            .update(cx, |s, cx| s.create_branch(name.clone(), start, cx));
        self._task = Some(cx.spawn(async move |this, cx| {
            let outcome = task.await;
            this.update(cx, |this, cx| {
                this.running = false;
                match outcome {
                    WriteOutcome::Committed(_) => {
                        this.session.update(cx, |s, cx| s.set_branch(name, cx));
                        cx.emit(DismissEvent);
                    }
                    WriteOutcome::Refused(error) => match &error.body.branch_start_refusal {
                        Some(refusal) => {
                            this.refused_start =
                                Some(start_refusal_notice(refusal, &error.body.error).into())
                        }
                        None => {
                            this.error = Some(format!("Not created: {}", error.reason()).into())
                        }
                    },
                    WriteOutcome::NotSent { reason } => {
                        this.error = Some(format!("Not created: {reason}").into())
                    }
                    WriteOutcome::Unknown { .. } => {
                        this.error = Some(
                            "The outcome is unknown. Check the branch list before trying again."
                                .into(),
                        );
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }
}

/// Why the server would not start a branch at a commit, in plain words, with what to do
/// instead; nothing was created. A reason this build does not know reads as the server's own
/// message, as any other refusal does.
fn start_refusal_notice(refusal: &BranchStartRefusalOutput, message: &str) -> String {
    let commit = short_commit_id(&refusal.graph_commit_id);
    match refusal.reason {
        BranchStartRefusalReason::NotRetained => format!(
            "Not created: cleanup no longer keeps {} as it was at {commit}. Start the branch at \
             a newer commit.",
            refusal.type_name.as_deref().unwrap_or("the graph")
        ),
        BranchStartRefusalReason::SchemaChanged => format!(
            "Not created: {commit} was written under an earlier schema. Start the branch at a \
             commit made since the schema last changed."
        ),
        BranchStartRefusalReason::EarlierStorageFormat => format!(
            "Not created: {commit} was written before the graph's storage was upgraded. Start \
             the branch at a commit made since the upgrade."
        ),
        BranchStartRefusalReason::BranchDeleted => format!(
            "Not created: {}, the branch that wrote {commit}, was deleted. Start the branch at a \
             commit of a branch that still exists.",
            refusal.branch.as_deref().unwrap_or("the branch")
        ),
        BranchStartRefusalReason::UnpublishedCommit => format!(
            "Not created: {commit} is not published yet. A branch can start at it once it is \
             published."
        ),
        BranchStartRefusalReason::UnattachedSnapshot => format!(
            "Not created: {commit} is an unattached snapshot, which never joins a branch's \
             history."
        ),
        BranchStartRefusalReason::Unknown | BranchStartRefusalReason::Other => {
            format!("Not created: {message}")
        }
    }
}

impl Render for CreateBranchModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let name = self.name.read(cx).text(cx);
        let name = if name.trim().is_empty() {
            "<name>"
        } else {
            name.trim()
        };
        // A commit start is headed by its short id, then who made it and when, as History
        // shows a commit; the line keeps its place while the commit is read.
        let (title, about, cli) = match &self.start {
            Start::Head(from) => (
                format!("New branch from {from}"),
                None,
                format!("omnigraph branch create {name} --from {from}"),
            ),
            Start::Commit { id, read } => {
                let about = match read {
                    CommitRead::Reading => "Reading the commit…".to_owned(),
                    CommitRead::Read(commit) => format!(
                        "{} · {}",
                        commit.actor_id.as_deref().unwrap_or("—"),
                        relative_time(commit.created_at, now_micros())
                    ),
                    CommitRead::Failed => {
                        "Who made the commit, and when, could not be read.".to_owned()
                    }
                };
                (
                    format!("New branch at {}", short_commit_id(id)),
                    Some(SharedString::from(about)),
                    format!("omnigraph branch create {name} --from-commit {id}"),
                )
            }
        };
        let refused = self.refused_start.is_some();
        ds::modal(cx)
            .key_context("CreateBranchModal")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .on_action(cx.listener(|this, _: &menu::Confirm, _, cx| this.create(cx)))
            .child(
                ds::ModalHeader::new(title)
                    .description(about)
                    .on_close(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
            )
            .child(
                ds::modal_body()
                    .child(field(&self.name, cx))
                    .child(studio_ui::ds::text::meta(cli).buffer_font(cx))
                    .children(self.error.clone().map(studio_ui::ds::text::error))
                    .children(self.refused_start.clone().map(studio_ui::ds::text::error)),
            )
            .child(
                ds::modal_footer(cx)
                    .child(
                        ds::cancel_button("cancel")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        ds::action_button(
                            "create",
                            if self.running {
                                "Creating…"
                            } else {
                                "Create"
                            },
                        )
                        .disabled(self.running || refused)
                        .when(refused, |button| {
                            button.tip(studio_ui::ds::tooltip(
                                "A branch cannot start at this commit",
                            ))
                        })
                        .on_click(cx.listener(|this, _, _, cx| this.create(cx))),
                    ),
            )
    }
}

/// What deleting a branch would lose, read from its merge base with [`TARGET`] when the
/// dialog opens.
#[derive(Debug, Clone, PartialEq)]
enum Holds {
    Reading,
    /// Its head is in the target: deleting it loses no commit.
    Merged,
    /// It has commits the target does not.
    Unmerged,
    /// The check could not be read: treated as unmerged.
    Unknown(String),
}

/// "Delete branch" (constitution IV). A branch whose every commit is already in `main` loses
/// nothing with it, so ↵ deletes it; one with commits only it holds, or one the check could not
/// read, is deleted by typing its name. `main` cannot be deleted.
pub struct DeleteBranchModal {
    session: Entity<GraphSession>,
    branch: String,
    holds: Holds,
    confirm: Entity<Editor>,
    error: Option<SharedString>,
    running: bool,
    focus_handle: FocusHandle,
    _check: Task<()>,
    _task: Option<Task<()>>,
    _subscription: Subscription,
}

impl EventEmitter<DismissEvent> for DeleteBranchModal {}

impl ModalView for DeleteBranchModal {
    /// A write still running keeps the dialog open: its outcome shows here.
    fn on_before_dismiss(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> bool {
        !self.running
    }
}

impl Focusable for DeleteBranchModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl DeleteBranchModal {
    pub fn new(
        session: Entity<GraphSession>,
        branch: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let confirm = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(&format!("Type {branch} to delete it"), window, cx);
            editor
        });
        let subscription = cx.subscribe(&confirm, |this, _, event: &EditorEvent, cx| {
            if let EditorEvent::BufferEdited = event {
                this.error = None;
                cx.notify();
            }
        });
        let read = session.update(cx, |session, cx| session.merge_base(&branch, TARGET, cx));
        let check = cx.spawn_in(window, async move |this, cx| {
            let holds = match read.await {
                Ok(base) if base.merge_base.graph_commit_id == base.source_commit_id => {
                    Holds::Merged
                }
                Ok(_) => Holds::Unmerged,
                Err(error) => Holds::Unknown(error.to_string()),
            };
            this.update_in(cx, |this, window, cx| {
                if holds != Holds::Merged {
                    window.focus(&this.confirm.focus_handle(cx), cx);
                }
                this.holds = holds;
                cx.notify();
            })
            .ok();
        });
        Self {
            session,
            branch,
            holds: Holds::Reading,
            confirm,
            error: None,
            running: false,
            focus_handle: cx.focus_handle(),
            _check: check,
            _task: None,
            _subscription: subscription,
        }
    }

    /// Whether Delete may run: a merged branch at once, any other once its name is typed.
    fn confirmed(&self, cx: &App) -> bool {
        self.branch != TARGET
            && match self.holds {
                Holds::Reading => false,
                Holds::Merged => true,
                Holds::Unmerged | Holds::Unknown(_) => {
                    self.confirm.read(cx).text(cx).trim() == self.branch
                }
            }
    }

    fn delete(&mut self, cx: &mut Context<Self>) {
        if self.running || !self.confirmed(cx) {
            return;
        }
        self.running = true;
        let branch = self.branch.clone();
        // Merged when the dialog opened, and asked again now: a commit landed on the branch
        // meanwhile is the branch's alone, and then the branch waits for its name.
        let recheck = matches!(self.holds, Holds::Merged).then(|| {
            self.session
                .update(cx, |session, cx| session.merge_base(&branch, TARGET, cx))
        });
        self._task =
            Some(cx.spawn(async move |this, cx| {
                if let Some(read) = recheck {
                    let holds = match read.await {
                        Ok(base) if base.merge_base.graph_commit_id == base.source_commit_id => {
                            Holds::Merged
                        }
                        Ok(_) => Holds::Unmerged,
                        Err(error) => Holds::Unknown(error.to_string()),
                    };
                    if !matches!(holds, Holds::Merged) {
                        this.update(cx, |this, cx| {
                            this.running = false;
                            this.error = Some(
                                match &holds {
                                    Holds::Unknown(error) => format!(
                                        "Studio could not check {} again: {}. Type its name to \
                                         delete it.",
                                        this.branch,
                                        studio_ui::clause(&error.to_string())
                                    ),
                                    _ => format!(
                                        "{} has a commit {TARGET} does not since this opened; \
                                         type its name to delete it.",
                                        this.branch
                                    ),
                                }
                                .into(),
                            );
                            this.holds = holds;
                            cx.notify();
                        })
                        .ok();
                        return;
                    }
                }
                let Ok(task) = this.update(cx, |this, cx| {
                    this.session.update(cx, |s, cx| s.delete_branch(branch, cx))
                }) else {
                    return;
                };
                let outcome = task.await;
                this.update(cx, |this, cx| {
                    this.running = false;
                    match outcome {
                        WriteOutcome::Committed(_) => cx.emit(DismissEvent),
                        WriteOutcome::Refused(error) => {
                            this.error = Some(format!("Not deleted: {}", error.reason()).into())
                        }
                        WriteOutcome::NotSent { reason } => {
                            this.error = Some(format!("Not deleted: {reason}").into())
                        }
                        WriteOutcome::Unknown { .. } => this.error = Some(
                            "The outcome is unknown. Check the branch list before trying again."
                                .into(),
                        ),
                    }
                    cx.notify();
                })
                .ok();
            }));
        cx.notify();
    }
}

impl Render for DeleteBranchModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let confirmed = self.confirmed(cx);
        let branch = &self.branch;
        let (what, typing) = match &self.holds {
            Holds::Reading => (
                format!("Checking whether {branch} is merged into {TARGET}…"),
                false,
            ),
            Holds::Merged => (
                format!("Everything on {branch} is in {TARGET}, so deleting it loses nothing."),
                false,
            ),
            Holds::Unmerged => (
                format!(
                    "{branch} has commits that are not in {TARGET}. Deleting it loses them, and \
                     Studio cannot bring them back."
                ),
                true,
            ),
            Holds::Unknown(error) => (
                format!(
                    "Studio could not tell whether {branch} is merged into {TARGET} ({error}). \
                     Deleting it may lose commits."
                ),
                true,
            ),
        };
        let cli = format!("omnigraph branch delete {branch}");
        let running = self.running;
        ds::modal(cx)
            .key_context("DeleteBranchModal")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .on_action(cx.listener(|this, _: &menu::Confirm, _, cx| this.delete(cx)))
            // What it loses is the description; the body holds only the field that asks for
            // the branch's name when it may lose commits.
            .child(
                ds::ModalHeader::new(format!("Delete {branch}?"))
                    .description(SharedString::from(what))
                    .meta(ds::write_target(
                        self.session.read(cx).write_scope(branch, cx),
                        cx,
                    ))
                    .close_disabled(running)
                    .on_close(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
            )
            .when(typing || self.error.is_some(), |dialog| {
                dialog.child(
                    ds::modal_body()
                        .when(typing, |body| body.child(field(&self.confirm, cx)))
                        .children(self.error.clone().map(studio_ui::ds::text::error)),
                )
            })
            .child(
                ds::modal_footer(cx)
                    .child(
                        ds::cancel_button("cancel")
                            .disabled(running)
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        ds::action_button(
                            "delete",
                            if self.running {
                                "Deleting…"
                            } else {
                                "Delete"
                            },
                        )
                        .variant(ds::Variant::Destructive)
                        .disabled(!confirmed || self.running)
                        .tip_fn(move |_, cx| {
                            studio_ui::ds::tooltip_with_meta(
                                "Delete the branch",
                                None,
                                cli.clone(),
                                cx,
                            )
                        })
                        .on_click(cx.listener(|this, _, _, cx| this.delete(cx))),
                    ),
            )
    }
}

/// Opens the delete dialog for `branch`, which reads again whether it is merged.
fn open_delete(
    workspace: &WeakEntity<Workspace>,
    session: Entity<GraphSession>,
    branch: &str,
    window: &mut Window,
    cx: &mut App,
) {
    let branch = branch.to_owned();
    workspace
        .update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                DeleteBranchModal::new(session, branch, window, cx)
            })
        })
        .ok();
}

/// Opens the dialog that picks what `from` is compared with (spec 043).
fn open_compare_dialog(
    workspace: &WeakEntity<Workspace>,
    session: Entity<GraphSession>,
    from: String,
    window: &mut Window,
    cx: &mut App,
) {
    workspace
        .update(cx, |workspace, cx| {
            workspace.show_modal(window, cx, |window, cx| {
                CompareModal::new(session, from, window, cx)
            })
        })
        .ok();
}

/// The buttons on the Branches panel's title line: compare, the grouping, and Refresh.
pub struct BranchesActions {
    list: WeakEntity<BranchList>,
    _observe: Subscription,
}

impl BranchesActions {
    pub fn new(list: &Entity<BranchList>, cx: &mut Context<Self>) -> Self {
        Self {
            list: list.downgrade(),
            _observe: cx.observe(list, |_, _, cx| cx.notify()),
        }
    }
}

impl Render for BranchesActions {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (grouping, author_offered) = self
            .list
            .upgrade()
            .map(|list| {
                let list = list.read(cx);
                let offered = list.session.as_ref().is_none_or(|session| {
                    session
                        .read(cx)
                        .features(cx)
                        .has(ServerFeature::DetailedBranchList)
                });
                (list.grouping_shown(cx), offered)
            })
            .unwrap_or((BranchGrouping::Prefix, true));
        let button = |id: &'static str, icon: IconName| {
            studio_ui::ds::row_icon_button(id, icon).icon_color(Color::Muted)
        };
        let (group_icon, group_tip) = match grouping {
            BranchGrouping::Prefix => (IconName::Person, "Group the branches by who wrote them"),
            BranchGrouping::Author => (
                IconName::ListTree,
                "Group the branches by the prefix of their names",
            ),
        };
        h_flex()
            .gap(px(studio_ui::ds::space::XXS))
            .child(
                button("branches-compare", IconName::Split)
                    .tip_fn(|_, cx| {
                        studio_ui::ds::tooltip_for_action_view(
                            "Compare the current branch with another",
                            &CompareBranches,
                            cx,
                        )
                    })
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(CompareBranches), cx)
                    }),
            )
            .child(
                button("branches-group", group_icon)
                    .disabled(!author_offered)
                    .tip_fn(move |_, cx| {
                        if author_offered {
                            studio_ui::ds::tooltip_for_action_view(
                                group_tip,
                                &ToggleBranchGrouping,
                                cx,
                            )
                        } else {
                            studio_ui::ds::tooltip(
                                "Grouping by author needs a server that lists every branch's \
                                 head (Omnigraph 0.15 or later)",
                            )
                            .into_view(cx)
                        }
                    })
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(ToggleBranchGrouping), cx)
                    }),
            )
            .child(
                button("branches-refresh", IconName::RotateCw)
                    .tip_fn(|_, cx| {
                        studio_ui::ds::tooltip_for_action_view(
                            "Read the branches again",
                            &RefreshBranches,
                            cx,
                        )
                    })
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(RefreshBranches), cx)
                    }),
            )
    }
}

/// Registers the branch actions on a workspace.
pub fn register(workspace: &mut Workspace) {
    // Compares the window's branch with one picked, or a commit (spec 043).
    workspace.register_action(|workspace, _: &CompareBranches, window, cx| {
        let Some(session) = workspace.session().cloned() else {
            return;
        };
        let from = session.read(cx).branch.clone();
        workspace.show_modal(window, cx, |window, cx| {
            CompareModal::new(session, from, window, cx)
        });
    });
    workspace.register_action(|workspace, action: &OpenCompare, window, cx| {
        open_compare(
            workspace,
            action.from.clone(),
            action.to.clone(),
            action.preview,
            window,
            cx,
        );
    });
    workspace.register_action(|workspace, _: &RefreshBranches, _, cx| {
        if let Some(session) = workspace.session().cloned() {
            session.update(cx, |session, cx| session.refresh_branches(cx));
        }
    });
    workspace.register_action(|_, _: &ToggleBranchGrouping, _, cx| {
        let grouping = StudioSettings::get(cx).studio.branch_grouping.other();
        studio_settings::update(cx, |settings| settings.studio.branch_grouping = grouping);
    });
    // A merge is made from the review, which holds it.
    workspace.register_action(|workspace, _: &ReviewBranch, window, cx| {
        let Some(session) = workspace.session() else {
            return;
        };
        let branch = session.read(cx).branch.clone();
        open_review(workspace, branch, TARGET.into(), window, cx);
    });
    workspace.register_action(|workspace, action: &OpenBranch, window, cx| {
        if action.branch == TARGET {
            window.dispatch_action(
                Box::new(OpenBranchHistory {
                    branch: action.branch.clone(),
                    preview: action.preview,
                }),
                cx,
            );
            return;
        }
        open_review_with(
            workspace,
            action.branch.clone(),
            TARGET.into(),
            action.preview,
            window,
            cx,
        );
    });
    workspace.register_action(|workspace, _: &CreateBranch, window, cx| {
        let Some(session) = workspace.session().cloned() else {
            return;
        };
        workspace.toggle_modal(window, cx, |window, cx| {
            CreateBranchModal::new(session, window, cx)
        });
    });
    // A start at a commit replaces whatever dialog is open, rather than closing one.
    workspace.register_action(|workspace, action: &CreateBranchAt, window, cx| {
        let Some(session) = workspace.session().cloned() else {
            return;
        };
        let commit = action.commit_id.clone();
        workspace.show_modal(window, cx, |window, cx| {
            CreateBranchModal::at_commit(session, commit, window, cx)
        });
    });
    // Not registered while the credential may not delete the window's branch, so the palette
    // does not offer it (constitution VIII).
    workspace.register_action_when(
        |workspace, cx| {
            workspace.session().is_none_or(|session| {
                let session = session.read(cx);
                let own = AccessScope::branch(&session.branch);
                session.refusal(AccessAction::BranchDelete, &own).is_none()
            })
        },
        delete_current,
    );
}

/// Deletes the window's branch, after the dialog asks.
fn delete_current(
    workspace: &mut Workspace,
    _: &DeleteBranch,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(session) = workspace.session().cloned() else {
        return;
    };
    let branch = session.read(cx).branch.clone();
    if branch == TARGET {
        workspace.show_toast(
            studio_workspace::ToastKind::Info,
            format!("{TARGET} cannot be deleted. Switch to another branch first."),
            cx,
        );
        return;
    }
    workspace.toggle_modal(window, cx, |window, cx| {
        DeleteBranchModal::new(session, branch, window, cx)
    });
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::view::tests::fake_server;

    /// The branches whose head was asked for, sorted: the reads go out together, in no set order.
    fn heads_asked(asked: &std::sync::Mutex<Vec<String>>) -> Vec<String> {
        let mut heads: Vec<String> = asked
            .lock()
            .unwrap()
            .iter()
            .filter_map(|request| {
                let (path, rest) = request.split_once('?')?;
                if !path.ends_with("/commits") {
                    return None;
                }
                let query = rest.split(' ').next()?;
                query
                    .split('&')
                    .find_map(|pair| pair.strip_prefix("branch="))
                    .map(str::to_owned)
            })
            .collect();
        heads.sort();
        heads
    }

    /// The sidebar's branches are a list that builds only the rows in view, however many
    /// branches the graph has: of 2,000, a dock of 320 pt builds a screenful, not every row and
    /// its menu at every frame, and reads the heads of those rows alone, once.
    #[gpui::test]
    async fn the_branch_list_builds_only_the_rows_in_view(cx: &mut gpui::TestAppContext) {
        cx.update(studio_theme::init_test);
        let names: Vec<String> = std::iter::once("main".to_owned())
            .chain((1..2_000).map(|i| format!("feature-{i:04}")))
            .collect();
        let (session, asked) = fake_server(
            move |path, _, _| {
                if path.ends_with("/branches") {
                    (200, json!({ "branches": names }))
                } else {
                    (404, json!({ "error": "not here" }))
                }
            },
            cx,
        );
        cx.run_until_parked();
        struct Dock(Entity<BranchList>);
        impl Render for Dock {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div().w(px(260.)).h(px(320.)).child(self.0.clone())
            }
        }
        let (dock, cx) = cx.add_window_view(|window, cx| {
            Dock(cx.new(|cx| BranchList::new(WeakEntity::new_invalid(), Some(session), window, cx)))
        });
        cx.run_until_parked();
        let list = dock.read_with(cx, |dock, _| dock.0.clone());
        list.update(cx, |list, cx| {
            list.rows_drawn = 0;
            cx.notify();
        });
        cx.run_until_parked();
        let drawn = list.read_with(cx, |list, _| list.rows_drawn);
        assert!(drawn > 0, "the rows in view are drawn");
        assert!(drawn < 100, "{drawn} rows built of 2,001");
        // The heads read are those of the first rows, the ones in view, each once however often
        // they are drawn (`drawn` also counts the row the list measures).
        let heads = heads_asked(&asked);
        assert!(!heads.is_empty() && heads.len() <= drawn, "{heads:?}");
        let mut in_view: Vec<String> = std::iter::once("main".to_owned())
            .chain((1..heads.len()).map(|i| format!("feature-{i:04}")))
            .collect();
        in_view.sort();
        assert_eq!(heads, in_view);
    }

    /// Draws the branch list, cached, while it shows it, as a dock draws its active panel.
    struct DockStandIn {
        list: Entity<BranchList>,
        shown: bool,
    }

    impl Render for DockStandIn {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().w(px(260.)).h(px(320.)).when(self.shown, |dock| {
                dock.child(
                    gpui::AnyView::from(self.list.clone())
                        .cached(gpui::StyleRefinement::default().size_full()),
                )
            })
        }
    }

    /// A branch list hidden in its dock reads no branch's head, as the window opens or when the
    /// branches change: each read takes one of the session's read slots from what is shown.
    /// Shown, it reads each head once; a change of the list while it is hidden is read when it
    /// is shown again, the heads the session holds answered without a request.
    #[gpui::test]
    async fn a_hidden_branch_list_reads_no_heads_until_shown(cx: &mut gpui::TestAppContext) {
        cx.update(studio_theme::init_test);
        let listed = std::sync::Arc::new(std::sync::Mutex::new(vec![
            "main".to_owned(),
            "feature".to_owned(),
            "spike".to_owned(),
        ]));
        let served = listed.clone();
        let (session, asked) = fake_server(
            move |path, query, _| {
                if path.ends_with("/branches") {
                    (200, json!({ "branches": *served.lock().unwrap() }))
                } else if path.ends_with("/commits") {
                    let branch = query
                        .split('&')
                        .find_map(|pair| pair.strip_prefix("branch="))
                        .unwrap_or_default();
                    let commit = json!({ "graph_commit_id": format!("{branch}-1"), "graph_manifest_version": 1, "kind": "published", "created_at": 0, "actor_id": "act-alice" });
                    (200, json!({ "commits": [commit] }))
                } else {
                    (404, json!({ "error": "not here" }))
                }
            },
            cx,
        );
        cx.run_until_parked();
        let held = session.clone();
        let (dock, cx) = cx.add_window_view(move |window, cx| {
            let list =
                cx.new(|cx| BranchList::new(WeakEntity::new_invalid(), Some(held), window, cx));
            DockStandIn { list, shown: false }
        });
        cx.run_until_parked();
        let show = |shown: bool, cx: &mut gpui::VisualTestContext| {
            dock.update(cx, |dock, cx| {
                dock.shown = shown;
                cx.notify();
            });
            cx.run_until_parked();
        };
        assert!(heads_asked(&asked).is_empty(), "nothing read while hidden");
        session.update(cx, |session, cx| session.reload_branches(cx));
        cx.run_until_parked();
        assert!(
            heads_asked(&asked).is_empty(),
            "nor when the branches change"
        );

        show(true, cx);
        assert_eq!(heads_asked(&asked), ["feature", "main", "spike"]);
        let list = dock.read_with(cx, |dock, _| dock.list.clone());
        list.read_with(cx, |list, _| {
            assert_eq!(list.details.len(), 3, "{:?}", list.details);
            assert_eq!(list.details["feature"].head, "feature-1");
        });
        // Drawn again, nothing is read again.
        list.update(cx, |list, cx| {
            list.rows_drawn = 0;
            cx.notify();
        });
        cx.run_until_parked();
        list.read_with(cx, |list, _| assert!(list.rows_drawn > 0, "drawn again"));
        assert_eq!(heads_asked(&asked).len(), 3);

        // A branch made while the list is hidden is read when it is shown, the others from
        // the heads the session holds.
        show(false, cx);
        listed.lock().unwrap().push("late".into());
        session.update(cx, |session, cx| session.reload_branches(cx));
        cx.run_until_parked();
        assert_eq!(heads_asked(&asked).len(), 3, "nothing read while hidden");
        show(true, cx);
        assert_eq!(heads_asked(&asked), ["feature", "late", "main", "spike"]);
        list.read_with(cx, |list, _| {
            assert_eq!(list.details["late"].head, "late-1");
            assert_eq!(list.details.len(), 4);
        });
    }

    /// The details shown are the shown graph's own: a switch to another graph while the first
    /// graph's heads are still being read shows none of them, as the other graph's branches
    /// are still to be read, and the other graph's heads once they are.
    #[gpui::test]
    async fn a_switch_of_graph_shows_none_of_the_previous_graphs_details(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(studio_theme::init_test);
        // Each graph's main has its own head, by its own actor.
        let graph = |head: &'static str, listed: std::sync::Arc<std::sync::atomic::AtomicBool>| {
            move |path: &str, _: &str, _: &str| {
                if path.ends_with("/branches") {
                    if listed.load(std::sync::atomic::Ordering::SeqCst) {
                        (200, json!({ "branches": ["main"] }))
                    } else {
                        (500, json!({ "error": "not yet" }))
                    }
                } else if path.ends_with("/commits") {
                    let commit = json!({ "graph_commit_id": head, "graph_manifest_version": 1, "kind": "published", "created_at": 0, "actor_id": format!("act-{head}") });
                    (200, json!({ "commits": [commit] }))
                } else {
                    (404, json!({ "error": "not here" }))
                }
            }
        };
        let listed = |now: bool| std::sync::Arc::new(std::sync::atomic::AtomicBool::new(now));
        let (first, _) = fake_server(graph("a1", listed(true)), cx);
        let second_listed = listed(false);
        let (second, _) = fake_server(graph("b1", second_listed.clone()), cx);
        struct Dock(Entity<BranchList>);
        impl Render for Dock {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div().w(px(260.)).h(px(320.)).child(self.0.clone())
            }
        }
        let (dock, cx) = cx.add_window_view(|window, cx| {
            Dock(cx.new(|cx| {
                let mut list = BranchList::new(WeakEntity::new_invalid(), Some(first), window, cx);
                // Switched before the first graph's heads are back.
                list.watch(Some(second.clone()), cx);
                list
            }))
        });
        cx.run_until_parked();
        let list = dock.read_with(cx, |dock, _| dock.0.clone());
        list.read_with(cx, |list, _| {
            assert!(list.details.is_empty(), "{:?}", list.details)
        });

        second_listed.store(true, std::sync::atomic::Ordering::SeqCst);
        second.update(cx, |session, cx| session.reload_branches(cx));
        cx.run_until_parked();
        list.read_with(cx, |list, _| {
            let main = list.details.get("main").expect("read");
            assert_eq!(main.head, "b1");
            assert_eq!(main.actor.as_deref(), Some("act-b1"));
        });
    }

    /// A merged branch is deleted with ↵; one with commits only it holds waits for its name.
    #[gpui::test]
    async fn a_merged_branch_deletes_at_once_and_an_unmerged_one_by_its_name(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let commit = |id: &str| json!({ "graph_commit_id": id, "graph_manifest_version": 1, "kind": "published", "created_at": 0 });
        let late_reads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let late = late_reads.clone();
        let (session, asked) = fake_server(
            move |path, query, _| {
                if path.ends_with("/branches/merge-base") {
                    // `feature`'s head is in main; `spike` has a commit of its own; `late` is
                    // merged when its dialog opens and has a commit of its own at the delete.
                    let late_merged = query.contains("source=late")
                        && late.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
                    let (base, head) = if query.contains("source=feature") || late_merged {
                        ("f1", "f1")
                    } else {
                        ("m1", "s1")
                    };
                    (
                        200,
                        json!({ "merge_base": commit(base), "source": "x", "source_commit_id": head, "target": "main", "target_commit_id": "m2" }),
                    )
                } else if let Some((_, branch)) = path.split_once("/branches/") {
                    // A branch delete's receipt, as the server answers it.
                    (
                        200,
                        json!({ "uri": "memory://knowledge", "name": branch, "actor_id": null }),
                    )
                } else {
                    (200, json!({ "branches": ["main", "feature", "spike"] }))
                }
            },
            cx,
        );
        let deletes = || {
            asked
                .lock()
                .unwrap()
                .iter()
                .filter(|request| {
                    request.contains("/branches/feature?")
                        || request.contains("/branches/spike?")
                        || request.contains("/branches/late?")
                })
                .count()
        };
        let (merged, cx) = cx.add_window_view(|window, cx| {
            DeleteBranchModal::new(session.clone(), "feature".into(), window, cx)
        });
        cx.run_until_parked();
        merged.update(cx, |modal, cx| {
            assert_eq!(modal.holds, Holds::Merged);
            modal.delete(cx);
        });
        cx.run_until_parked();
        assert_eq!(deletes(), 1, "deleted with no name typed");
        merged.read_with(cx, |modal, _| {
            assert_eq!(modal.error, None, "the receipt settles the delete")
        });

        let (unmerged, cx) = cx.add_window_view(|window, cx| {
            DeleteBranchModal::new(session.clone(), "spike".into(), window, cx)
        });
        cx.run_until_parked();
        unmerged.update(cx, |modal, cx| {
            assert_eq!(modal.holds, Holds::Unmerged);
            modal.delete(cx);
        });
        cx.run_until_parked();
        assert_eq!(deletes(), 1, "not without its name");
        unmerged.update_in(cx, |modal, window, cx| {
            modal
                .confirm
                .update(cx, |editor, cx| editor.set_text("spike", window, cx));
            modal.delete(cx);
        });
        cx.run_until_parked();
        assert_eq!(deletes(), 2);

        let (late, cx) = cx.add_window_view(|window, cx| {
            DeleteBranchModal::new(session.clone(), "late".into(), window, cx)
        });
        cx.run_until_parked();
        late.update(cx, |modal, cx| {
            assert_eq!(modal.holds, Holds::Merged);
            modal.delete(cx);
        });
        cx.run_until_parked();
        assert_eq!(
            deletes(),
            2,
            "a commit landed meanwhile: not without its name"
        );
        late.read_with(cx, |modal, _| {
            assert_eq!(modal.holds, Holds::Unmerged);
            assert!(modal.error.is_some());
        });
    }

    /// A branch at a commit is asked for with the commit alone, never a branch too, and the
    /// dialog names who made the commit and when. Each refusal to start there says why in
    /// words, naming the type or the branch it is about, and nothing is sent again: not by
    /// itself, and not by Create, which stays off while the name changes. A reason this build
    /// does not know reads as the server's message.
    #[gpui::test]
    async fn a_branch_at_a_commit_says_why_it_was_refused_and_is_not_sent_again(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        // What each commit's start is refused with: the reason, its detail and its status.
        let refusals = [
            ("c1", "not_retained", json!({ "type_name": "Person" }), 410),
            ("c2", "schema_changed", json!({}), 409),
            ("c3", "earlier_storage_format", json!({}), 409),
            ("c4", "branch_deleted", json!({ "branch": "spike" }), 409),
            ("c5", "unpublished_commit", json!({}), 409),
            ("c6", "unattached_snapshot", json!({}), 409),
            ("c7", "unknown", json!({}), 409),
            ("c8", "a_later_reason", json!({}), 409),
        ];
        let notices = [
            "Not created: cleanup no longer keeps Person as it was at c1. Start the branch at a \
             newer commit.",
            "Not created: c2 was written under an earlier schema. Start the branch at a commit \
             made since the schema last changed.",
            "Not created: c3 was written before the graph's storage was upgraded. Start the \
             branch at a commit made since the upgrade.",
            "Not created: spike, the branch that wrote c4, was deleted. Start the branch at a \
             commit of a branch that still exists.",
            "Not created: c5 is not published yet. A branch can start at it once it is \
             published.",
            "Not created: c6 is an unattached snapshot, which never joins a branch's history.",
            "Not created: cannot start a branch at commit 'c7'",
            "Not created: cannot start a branch at commit 'c8'",
        ];
        let sent = std::sync::Arc::new(std::sync::Mutex::new(Vec::<serde_json::Value>::new()));
        let creates = sent.clone();
        let served = refusals.clone();
        // The branches the server lists: those it made, once it makes them.
        let made = std::sync::Arc::new(std::sync::Mutex::new(vec![
            "main".to_owned(),
            "feature".to_owned(),
        ]));
        let (session, _) = fake_server(
            move |path, _, body| {
                if let Some(id) = path.rsplit_once("/commits/").map(|(_, id)| id) {
                    let commit = json!({ "graph_commit_id": id, "graph_manifest_version": 1, "kind": "published", "created_at": 0, "actor_id": "act-alice" });
                    return (200, commit);
                }
                if !path.ends_with("/branches") || body.is_empty() {
                    return (200, json!({ "branches": *made.lock().unwrap() }));
                }
                let asked: serde_json::Value = serde_json::from_str(body).unwrap();
                creates.lock().unwrap().push(asked.clone());
                let at = asked["from_commit"].as_str().unwrap_or_default().to_owned();
                match served.iter().find(|(commit, ..)| *commit == at) {
                    Some((commit, reason, detail, status)) => {
                        let mut refusal = json!({ "reason": reason, "graph_commit_id": commit });
                        for (key, value) in detail.as_object().unwrap() {
                            refusal[key] = value.clone();
                        }
                        let error = format!("cannot start a branch at commit '{commit}'");
                        (
                            *status,
                            json!({ "error": error, "code": "branch_start_refusal", "disposition": { "action": "fix", "effects": "none" }, "branch_start_refusal": refusal }),
                        )
                    }
                    None => {
                        let name = asked["name"].as_str().unwrap_or_default().to_owned();
                        made.lock().unwrap().push(name.clone());
                        (
                            200,
                            json!({ "uri": "u", "from": "main", "name": name, "from_commit_id": at }),
                        )
                    }
                }
            },
            cx,
        );
        for ((commit, ..), notice) in refusals.iter().zip(notices) {
            let (modal, cx) = cx.add_window_view(|window, cx| {
                CreateBranchModal::at_commit(session.clone(), (*commit).to_owned(), window, cx)
            });
            cx.run_until_parked();
            modal.update_in(cx, |modal, window, cx| {
                assert!(matches!(
                    &modal.start,
                    Start::Commit { read: CommitRead::Read(read), .. }
                        if read.actor_id.as_deref() == Some("act-alice")
                ));
                modal
                    .name
                    .update(cx, |editor, cx| editor.set_text("then", window, cx));
                modal.create(cx);
            });
            cx.run_until_parked();
            modal.update_in(cx, |modal, window, cx| {
                assert_eq!(modal.refused_start.as_deref(), Some(notice), "{commit}");
                assert_eq!(modal.error, None);
                // The refusal is the commit's: another name keeps it, and sends nothing.
                modal
                    .name
                    .update(cx, |editor, cx| editor.set_text("again", window, cx));
                modal.create(cx);
            });
            cx.run_until_parked();
            modal.read_with(cx, |modal, _| {
                assert_eq!(modal.refused_start.as_deref(), Some(notice));
                assert!(!modal.running);
            });
        }
        assert_eq!(
            sent.lock().unwrap().len(),
            refusals.len(),
            "each start was sent once"
        );

        // A start the server takes: the commit alone is sent, and the session moves there.
        let (modal, cx) = cx.add_window_view(|window, cx| {
            CreateBranchModal::at_commit(session.clone(), "c9".into(), window, cx)
        });
        cx.run_until_parked();
        modal.update_in(cx, |modal, window, cx| {
            modal
                .name
                .update(cx, |editor, cx| editor.set_text("then", window, cx));
            modal.create(cx);
        });
        cx.run_until_parked();
        assert!(
            sent.lock()
                .unwrap()
                .iter()
                .all(|asked| asked.get("from").is_none())
        );
        assert_eq!(
            sent.lock().unwrap().last(),
            Some(&json!({ "name": "then", "from_commit": "c9" }))
        );
        session.read_with(cx, |session, _| {
            assert_eq!(session.branch, "then");
            assert_eq!(session.head("then"), Some("c9"));
        });
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| (*name).to_owned()).collect()
    }

    /// What a layout's rows say, as the list draws them: a branch by the name it shows,
    /// inset under its group; a group by its label and count.
    fn rows_of(layout: &Layout, branches: &[String]) -> Vec<String> {
        layout
            .rows
            .iter()
            .map(|row| match row {
                ListRow::Branch {
                    index,
                    shown_from,
                    grouped,
                } => format!(
                    "{}{}",
                    if *grouped { "  " } else { "" },
                    &branches[*index][*shown_from..]
                ),
                ListRow::Group(group) => {
                    let group = &layout.groups[*group];
                    format!("{} ({})", group.label, group.branches)
                }
                ListRow::NewBranch => "New branch…".to_owned(),
            })
            .collect()
    }

    /// By prefix (spec 043): main first, then the branches whose prefix no other shares, then
    /// a group for each prefix two or more share, its branches named without it; a folded group
    /// keeps its header and hides its branches.
    #[test]
    fn branches_group_by_the_prefix_two_or_more_share() {
        let branches = names(&[
            "main",
            "agent/notes-oct-14",
            "agent/slack-oct-14",
            "fix-titles",
            "plan/sharing-v2",
            "tentative/ingest-1117",
            "tentative/scout-helix",
        ]);
        let layout = lay_out(&branches, BranchGrouping::Prefix, |_| None, &HashSet::new());
        assert_eq!(
            rows_of(&layout, &branches),
            [
                "main",
                "fix-titles",
                "plan/sharing-v2",
                "agent/ (2)",
                "  notes-oct-14",
                "  slack-oct-14",
                "tentative/ (2)",
                "  ingest-1117",
                "  scout-helix",
                "New branch…",
            ]
        );
        let folded = HashSet::from(["prefix:agent".to_owned()]);
        let layout = lay_out(&branches, BranchGrouping::Prefix, |_| None, &folded);
        assert_eq!(
            rows_of(&layout, &branches)[3..6],
            ["agent/ (2)", "tentative/ (2)", "  ingest-1117"]
        );
        assert!(!layout.groups[0].open);
    }

    /// By author: a group for each author, by name, every branch in one, those whose author is
    /// not known last; main stays first.
    #[test]
    fn branches_group_by_author_with_the_unknown_last() {
        let branches = names(&["main", "agent/a", "agent/b", "plan/x", "secret"]);
        let author = |branch: &str| match branch {
            "agent/a" | "plan/x" => Some("act-notes-bot".to_owned()),
            "agent/b" => Some("act-alice".to_owned()),
            _ => None,
        };
        let layout = lay_out(&branches, BranchGrouping::Author, author, &HashSet::new());
        assert_eq!(
            rows_of(&layout, &branches),
            [
                "main",
                "act-alice (1)",
                "  agent/b",
                "act-notes-bot (2)",
                "  agent/a",
                "  plan/x",
                "Author not known (1)",
                "  secret",
                "New branch…",
            ]
        );
    }

    /// A branch's commits ahead of main stop at the merge base or at the first commit written
    /// on main; past a full page, there may be more.
    #[test]
    fn a_branch_is_ahead_by_its_commits_down_to_main() {
        let commit = |id: &str, branch: Option<&str>| {
            serde_json::from_value::<CommitOutput>(json!({ "graph_commit_id": id, "graph_manifest_version": 1, "kind": "published", "created_at": 0, "graph_branch": branch })).unwrap()
        };
        let own = [
            commit("s3", Some("spike")),
            commit("s2", Some("spike")),
            commit("s1", Some("spike")),
        ];
        let mut history = own.to_vec();
        history.push(commit("m1", None));
        assert_eq!(
            ahead_of(&history, "m1", true),
            Lane::Ahead {
                commits: 3,
                more: false
            }
        );
        // From a branch of a branch: the other branch's commits count until main's.
        assert_eq!(
            ahead_of(&history, "x", true),
            Lane::Ahead {
                commits: 3,
                more: false
            }
        );
        // Merged once at s2: only what came after.
        assert_eq!(
            ahead_of(&history, "s2", true),
            Lane::Ahead {
                commits: 1,
                more: false
            }
        );
        assert_eq!(
            ahead_of(&own, "m1", false),
            Lane::Ahead {
                commits: 3,
                more: true
            }
        );
    }

    /// A server whose `feature` is merged into main and whose `spike` has two commits of its
    /// own; every request is logged.
    fn laned_server(
        cx: &mut gpui::TestAppContext,
    ) -> (
        Entity<GraphSession>,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        let commit = |id: &str, branch: Option<&str>| json!({ "graph_commit_id": id, "graph_manifest_version": 1, "kind": "published", "created_at": 0, "graph_branch": branch, "actor_id": "act-agent" });
        fake_server(
            move |path, query, _| {
                let side = |name: &str| {
                    query
                        .split('&')
                        .find_map(|pair| pair.strip_prefix(&format!("{name}=")))
                        .unwrap_or_default()
                        .to_owned()
                };
                if path.ends_with("/branches/merge-base") {
                    let (base, head) = match side("source").as_str() {
                        "feature" => ("f1", "f1"),
                        _ => ("m1", "s2"),
                    };
                    (
                        200,
                        json!({ "merge_base": commit(base, None), "source": side("source"), "source_commit_id": head, "target": "main", "target_commit_id": "m2" }),
                    )
                } else if path.ends_with("/commits") {
                    let commits = match side("branch").as_str() {
                        "spike" => vec![
                            commit("s2", Some("spike")),
                            commit("s1", Some("spike")),
                            commit("m1", None),
                        ],
                        "feature" => vec![commit("f1", Some("feature"))],
                        _ => vec![commit("m2", None)],
                    };
                    (200, json!({ "commits": commits }))
                } else if path.ends_with("/branches") {
                    (200, json!({ "branches": ["main", "feature", "spike"] }))
                } else {
                    (404, json!({ "error": "not here" }))
                }
            },
            cx,
        )
    }

    fn requests(asked: &std::sync::Mutex<Vec<String>>, part: &str) -> usize {
        asked
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.contains(part))
            .count()
    }

    /// Each drawn branch's lane (spec 043): a branch whose head is in main is merged, one with
    /// commits of its own is ahead by them; main has none. A commit to main asks every drawn
    /// lane again, and answers alike do not draw the list again.
    #[gpui::test]
    async fn drawn_branches_are_marked_merged_or_ahead_and_read_again_when_main_moves(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(studio_theme::init_test);
        let (session, asked) = laned_server(cx);
        cx.run_until_parked();
        struct Dock(Entity<BranchList>);
        impl Render for Dock {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div().w(px(260.)).h(px(320.)).child(self.0.clone())
            }
        }
        let held = session.clone();
        let (dock, cx) = cx.add_window_view(|window, cx| {
            Dock(cx.new(|cx| BranchList::new(WeakEntity::new_invalid(), Some(held), window, cx)))
        });
        cx.run_until_parked();
        let list = dock.read_with(cx, |dock, _| dock.0.clone());
        list.read_with(cx, |list, _| {
            assert_eq!(list.lanes.get("feature"), Some(&Lane::Merged));
            assert_eq!(
                list.lanes.get("spike"),
                Some(&Lane::Ahead {
                    commits: 2,
                    more: false
                })
            );
            assert_eq!(list.lanes.get("main"), None);
        });
        assert_eq!(requests(&asked, "merge-base?"), 2, "{asked:?}");
        assert_eq!(requests(&asked, "source=main"), 0);
        // Merged, a branch's history is not read for its lane.
        assert_eq!(requests(&asked, "branch=feature&limit=100"), 0, "{asked:?}");
        assert_eq!(requests(&asked, "branch=spike&limit=100"), 1, "{asked:?}");

        list.read_with(cx, |list, _| {
            assert_eq!(list.lane_draws, 2, "one draw a new mark")
        });
        session.update(cx, |_, cx| {
            cx.emit(SessionEvent::Committed {
                branch: TARGET.to_owned(),
            })
        });
        cx.run_until_parked();
        assert_eq!(requests(&asked, "merge-base?"), 4, "both read again");
        list.read_with(cx, |list, _| {
            assert_eq!(list.lane_draws, 2, "the same answers draw nothing")
        });
    }

    /// The panel's Refresh reads the branch list again, and every drawn branch's head and lane,
    /// so a branch an agent made shows without waiting for the poll.
    #[gpui::test]
    async fn refresh_reads_the_branches_their_heads_and_lanes_again(cx: &mut gpui::TestAppContext) {
        cx.update(studio_theme::init_test);
        let (session, asked) = laned_server(cx);
        cx.run_until_parked();
        struct Dock(Entity<BranchList>);
        impl Render for Dock {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div().w(px(260.)).h(px(320.)).child(self.0.clone())
            }
        }
        let held = session.clone();
        let (_dock, cx) = cx.add_window_view(|window, cx| {
            Dock(cx.new(|cx| BranchList::new(WeakEntity::new_invalid(), Some(held), window, cx)))
        });
        cx.run_until_parked();
        let lists = requests(&asked, "/branches? ");
        let heads = requests(&asked, "&limit=1 ") + requests(&asked, "limit=1&");
        let lanes = requests(&asked, "merge-base?");
        session.update(cx, |session, cx| session.refresh_branches(cx));
        cx.run_until_parked();
        assert_eq!(requests(&asked, "/branches? "), lists + 1, "{asked:?}");
        assert!(
            requests(&asked, "&limit=1 ") + requests(&asked, "limit=1&") > heads,
            "heads read again: {asked:?}"
        );
        assert_eq!(requests(&asked, "merge-base?"), lanes + 2, "{asked:?}");
    }

    /// Grouped, the list still builds only the rows in view (`docs/drawing.md`): of 2,000
    /// branches under four prefixes, a screenful, and reads the lanes of those alone.
    #[gpui::test]
    async fn a_grouped_list_builds_only_the_rows_in_view(cx: &mut gpui::TestAppContext) {
        cx.update(studio_theme::init_test);
        let names: Vec<String> = std::iter::once("main".to_owned())
            .chain((1..2_000).map(|i| {
                let prefix = ["agent", "capture", "intake", "tentative"][i % 4];
                format!("{prefix}/run-{i:04}")
            }))
            .collect();
        let (session, asked) = fake_server(
            move |path, _, _| {
                if path.ends_with("/branches") {
                    (200, json!({ "branches": names }))
                } else {
                    (404, json!({ "error": "not here" }))
                }
            },
            cx,
        );
        cx.run_until_parked();
        struct Dock(Entity<BranchList>);
        impl Render for Dock {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div().w(px(260.)).h(px(320.)).child(self.0.clone())
            }
        }
        let (dock, cx) = cx.add_window_view(|window, cx| {
            Dock(cx.new(|cx| BranchList::new(WeakEntity::new_invalid(), Some(session), window, cx)))
        });
        cx.run_until_parked();
        let list = dock.read_with(cx, |dock, _| dock.0.clone());
        list.update(cx, |list, cx| {
            list.rows_drawn = 0;
            cx.notify();
        });
        cx.run_until_parked();
        let (drawn, rows, groups) = list.read_with(cx, |list, _| {
            let layout = list.layout.clone().unwrap();
            (list.rows_drawn, layout.rows.len(), layout.groups.len())
        });
        assert_eq!(groups, 4);
        assert_eq!(
            rows,
            2_000 + 4 + 1,
            "every branch, four headers, New branch…"
        );
        assert!(drawn > 0 && drawn < 100, "{drawn} rows built of {rows}");
        let lanes = requests(&asked, "merge-base?");
        assert!(lanes > 0 && lanes < 100, "{lanes} lanes read");
    }

    /// Grouping by author needs every head at once, which only a server that details its
    /// branches answers in one read (Omnigraph 0.15): elsewhere the list groups by prefix and
    /// asks for no detailed list. Where it is served, the rows wait for that list, then group
    /// by who wrote each branch's newest commit.
    #[gpui::test]
    async fn grouping_by_author_waits_for_one_detailed_list_where_it_is_served(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(studio_theme::init_test);
        let detail = |name: &str, actor: &str| json!({ "name": name, "updated_at": 0, "head": { "graph_commit_id": format!("{name}-1"), "graph_manifest_version": 1, "kind": "published", "created_at": 0, "actor_id": actor } });
        let details = json!([
            detail("main", "act-admin"),
            detail("agent/a", "act-notes-bot"),
            detail("agent/b", "act-slack-bot"),
            detail("plan/x", "act-notes-bot"),
        ]);
        let (session, asked) = fake_server(
            move |path, query, _| {
                if path.ends_with("/branches") {
                    let mut list = json!({ "branches": ["main", "agent/a", "agent/b", "plan/x"] });
                    if query.contains("detailed=true") {
                        list["details"] = details.clone();
                    }
                    (200, list)
                } else {
                    (404, json!({ "error": "not here" }))
                }
            },
            cx,
        );
        cx.run_until_parked();
        struct Dock(Entity<BranchList>);
        impl Render for Dock {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div().w(px(260.)).h(px(320.)).child(self.0.clone())
            }
        }
        let held = session.clone();
        let (dock, cx) = cx.add_window_view(|window, cx| {
            Dock(cx.new(|cx| BranchList::new(WeakEntity::new_invalid(), Some(held), window, cx)))
        });
        cx.run_until_parked();
        let list = dock.read_with(cx, |dock, _| dock.0.clone());
        let labels = |cx: &mut gpui::VisualTestContext| {
            list.update(cx, |list, cx| {
                list.layout(cx)
                    .map(|layout| layout.groups.iter().map(|g| g.label.clone()).collect())
                    .unwrap_or_else(Vec::<String>::new)
            })
        };
        list.update(cx, |list, cx| {
            list.grouping = BranchGrouping::Author;
            list.relayout(cx);
            assert_eq!(
                list.grouping_shown(cx),
                BranchGrouping::Prefix,
                "a 0.14 server"
            );
        });
        cx.run_until_parked();
        assert_eq!(labels(cx), ["agent/"], "by prefix");
        // Heads read alone as rows are drawn, never the detailed list.
        assert_eq!(requests(&asked, "detailed=true"), 0, "{asked:?}");

        let connection = session.read_with(cx, |session, _| session.connection.clone());
        connection.update(cx, |connection, cx| {
            connection.set_features(graph_store::ServerFeatures::of(Some("0.15.0")), cx)
        });
        session.update(cx, |session, cx| session.reload_branches(cx));
        cx.run_until_parked();
        list.update(cx, |list, cx| {
            assert_eq!(list.grouping_shown(cx), BranchGrouping::Author);
            assert!(list._authors_read.is_none(), "read");
        });
        assert_eq!(labels(cx), ["act-notes-bot", "act-slack-bot"]);
        assert!(requests(&asked, "detailed=true") >= 1, "{asked:?}");
    }
}
