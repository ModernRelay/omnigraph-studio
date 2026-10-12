//! The history tab (spec 009): a branch's commits newest first, a lane graph for merges, and
//! the selected commit's changes in a detail split.

use std::collections::HashMap;
use std::ops::Range;

use gpui::{
    AnyElement, App, Bounds, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable,
    Hsla, PathBuilder, Pixels, Render, ScrollStrategy, SharedString, Subscription, Task,
    UniformListScrollHandle, Window, canvas, point, px, uniform_list,
};
use graph_client::api::CommitOutput;
use graph_store::GraphSession;
use studio_actions::graph::OpenCommit;
use studio_ui::{change_counts, empty_state, now_micros, relative_time, short_commit_id};
use studio_workspace::{Item, ItemEvent};
use ui::prelude::*;
use ui::{Banner, ContextMenu, Severity, right_click_menu};

use crate::commit_view::{CommitCounts, CommitView};
use crate::lanes::{Graph, GraphCommit, assign_lanes};
use crate::merged::{branch_heads, with_merged_commits};

/// Above this many commits the list warns that it loads in full (E10).
pub const LONG_HISTORY: usize = 5_000;

/// Other branches whose lists History reads for heads and merged-in commits.
const OTHER_BRANCHES: usize = 20;

const ROW_HEIGHT: f32 = 28.;
const LANE_WIDTH: f32 = 14.;
const GRAPH_PADDING: f32 = 8.;

enum State {
    Loading,
    Ready,
    Failed(SharedString),
}

pub struct HistoryView {
    session: Entity<GraphSession>,
    branch: String,
    focus_handle: FocusHandle,
    state: State,
    commits: Vec<CommitOutput>,
    graph: Graph,
    selected: Option<usize>,
    detail: Option<Entity<CommitView>>,
    counts: HashMap<String, (usize, usize, usize)>,
    /// Branch names by the commit they point at.
    heads: HashMap<String, Vec<String>>,
    /// Commits in the branch's own list; the rest came in through merges.
    own_count: usize,
    scroll: UniformListScrollHandle,
    _task: Option<Task<()>>,
    _detail_subscription: Option<Subscription>,
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
        Some(IconName::GitGraph)
    }

    fn dedupe_key(&self, _cx: &App) -> Option<SharedString> {
        Some(history_key(&self.branch).into())
    }
}

pub fn history_key(branch: &str) -> String {
    format!("history:{branch}")
}

impl HistoryView {
    pub fn new(session: Entity<GraphSession>, branch: String, cx: &mut Context<Self>) -> Self {
        let mut view = Self {
            session,
            branch,
            focus_handle: cx.focus_handle(),
            state: State::Loading,
            commits: Vec::new(),
            graph: Graph::default(),
            selected: None,
            detail: None,
            counts: HashMap::new(),
            heads: HashMap::new(),
            own_count: 0,
            scroll: UniformListScrollHandle::new(),
            _task: None,
            _detail_subscription: None,
        };
        view.reload(cx);
        view
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.state = State::Loading;
        let client = self.session.read(cx).client(cx);
        let graph = self.session.read(cx).graph_id.clone();
        let branch = self.branch.clone();
        self._task = Some(cx.spawn(async move |this, cx| {
            let own = match client.commits(&graph, &branch).await {
                Ok(list) => list.commits,
                Err(error) => {
                    this.update(cx, |this, cx| {
                        this.state = State::Failed(error.to_string().into());
                        cx.notify();
                    })
                    .ok();
                    return;
                }
            };
            // Other live branches: their heads, and the commits merged in from them (E9).
            let mut others: Vec<(String, Vec<CommitOutput>)> = Vec::new();
            if let Ok(list) = client.branches(&graph).await {
                for name in list
                    .branches
                    .into_iter()
                    .filter(|name| *name != branch)
                    .take(OTHER_BRANCHES)
                {
                    if let Ok(commits) = client.commits(&graph, &name).await {
                        others.push((name, commits.commits));
                    }
                }
            }
            // Lay out off the main thread; long histories load in full (E10).
            let own_count = own.len();
            let (commits, layout, heads) = cx
                .background_executor()
                .spawn(async move {
                    let heads = branch_heads(
                        std::iter::once((branch.as_str(), own.as_slice())).chain(
                            others
                                .iter()
                                .map(|(name, commits)| (name.as_str(), commits.as_slice())),
                        ),
                    );
                    let lists: Vec<Vec<CommitOutput>> =
                        others.into_iter().map(|(_, commits)| commits).collect();
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

    /// Selects a commit and shows its changes in the detail split.
    pub fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(commit) = self.commits.get(ix).cloned() else {
            return;
        };
        self.selected = Some(ix);
        self.scroll.scroll_to_item(ix, ScrollStrategy::Center);
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

    /// Selects the commit with this id, when it is in the list.
    pub fn select_commit(&mut self, commit_id: &str, cx: &mut Context<Self>) -> bool {
        match self
            .commits
            .iter()
            .position(|c| c.graph_commit_id == commit_id)
        {
            Some(ix) => {
                self.select(ix, cx);
                true
            }
            None => false,
        }
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

    fn render_rows(
        &mut self,
        range: Range<usize>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let colors = cx.theme().colors();
        let selected_bg = colors.element_selected;
        let hover_bg = colors.element_hover;
        let accents = cx.theme().accents().clone();
        let graph_width = self.graph_width();
        let now = now_micros();
        range
            .filter_map(|ix| {
                let commit = self.commits.get(ix)?;
                let row = self.graph.rows.get(ix)?.clone();
                let color = |index: usize| accents.color_for_index(index as u32);
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
                let heads = self.heads.get(&id).cloned().unwrap_or_default();
                let selected = self.selected == Some(ix);
                let actor = commit.actor_id.clone();
                let open_id = id.clone();
                let copy_id = id.clone();
                let row_element = h_flex()
                    .id(("commit-row", ix))
                    .h(px(ROW_HEIGHT))
                    .w_full()
                    .pr_2()
                    .when(selected, |d| d.bg(selected_bg))
                    .hover(|d| d.bg(hover_bg))
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
                    .child(
                        div().w(px(86.)).flex_none().child(
                            Label::new(short_commit_id(&id))
                                .size(LabelSize::Small)
                                .buffer_font(cx)
                                .color(if selected {
                                    Color::Default
                                } else {
                                    Color::Muted
                                }),
                        ),
                    )
                    .child(
                        div().w(px(120.)).flex_none().child(
                            Label::new(actor.unwrap_or_else(|| "—".into()))
                                .size(LabelSize::Small)
                                .single_line()
                                .truncate(),
                        ),
                    )
                    .child(
                        div().w(px(96.)).flex_none().child(
                            Label::new(relative_time(commit.created_at, now))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                    )
                    .child(
                        h_flex()
                            .w(px(170.))
                            .flex_none()
                            .gap_1()
                            .when(heads.is_empty(), |d| {
                                d.child(chip(branch, Color::Muted, cx))
                            })
                            .children(heads.iter().map(|name| {
                                let color = if *name == self.branch {
                                    Color::Accent
                                } else {
                                    Color::Default
                                };
                                chip(format!("{name} ▸"), color, cx)
                            }))
                            .when(commit.merged_parent_commit_id.is_some(), |d| {
                                d.child(chip("merge".into(), Color::Info, cx))
                            }),
                    )
                    .child(div().flex_1().min_w_0().children(counts.map(
                        |(added, changed, removed)| {
                            if added + changed + removed == 0 {
                                Label::new("no changes")
                                    .size(LabelSize::XSmall)
                                    .color(Color::Disabled)
                                    .into_any_element()
                            } else {
                                change_counts(added, changed, removed).into_any_element()
                            }
                        },
                    )))
                    .on_click(
                        cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                            this.select(ix, cx);
                            if event.click_count() == 2 {
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
                                let copy_id = copy_id.clone();
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
        let colors = cx.theme().colors();
        let header_bg = colors.panel_background;
        let border = colors.border_variant;
        let graph_width = self.graph_width();
        let column = |title: &'static str, width: Option<f32>| {
            let label = Label::new(title)
                .size(LabelSize::XSmall)
                .color(Color::Muted);
            match width {
                Some(width) => div().w(px(width)).flex_none().child(label),
                None => div().flex_1().child(label),
            }
        };
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .h(px(24.))
                    .flex_none()
                    .pr_2()
                    .bg(header_bg)
                    .border_b_1()
                    .border_color(border)
                    .child(column("Graph", Some(graph_width)).pl_2())
                    .child(column("Commit", Some(86.)))
                    .child(column("Actor", Some(120.)))
                    .child(column("When", Some(96.)))
                    .child(column("Branch", Some(170.)))
                    .child(column("Changes", None)),
            )
            .child(
                uniform_list(
                    "history-rows",
                    self.commits.len(),
                    cx.processor(|this, range, window, cx| this.render_rows(range, window, cx)),
                )
                .track_scroll(&self.scroll)
                .flex_1()
                .min_h_0(),
            )
            .into_any_element()
    }
}

/// A small rounded label.
fn chip(text: String, color: Color, cx: &App) -> AnyElement {
    let border = cx.theme().colors().border_variant;
    div()
        .px_1p5()
        .rounded_sm()
        .border_1()
        .border_color(border)
        .child(
            Label::new(text)
                .size(LabelSize::XSmall)
                .color(color)
                .single_line()
                .truncate(),
        )
        .into_any_element()
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let border = cx.theme().colors().border_variant;
        let count = self.commits.len();
        let header = h_flex()
            .flex_none()
            .px_4()
            .py_2()
            .gap_2()
            .border_b_1()
            .border_color(border)
            .child(Icon::new(IconName::GitGraph).color(Color::Muted))
            .child(Headline::new(format!("History of {}", self.branch)).size(HeadlineSize::XSmall))
            .child(
                Label::new(match self.state {
                    State::Ready if count > self.own_count => format!(
                        "{count} commits · {} from merged branches",
                        count - self.own_count
                    ),
                    State::Ready => format!("{count} commit{}", if count == 1 { "" } else { "s" }),
                    State::Loading => "Reading commits…".into(),
                    State::Failed(_) => String::new(),
                })
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .child(div().flex_1())
            .child(
                Button::new("refresh-history", "Refresh")
                    .style(ButtonStyle::Subtle)
                    .label_size(LabelSize::Small)
                    .start_icon(Icon::new(IconName::RotateCw).size(IconSize::Small))
                    .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
            );
        let warning = (count > LONG_HISTORY).then(|| {
            div().px_4().py_1().child(
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
                empty_state("History could not be read.", Some(error.clone())).into_any_element()
            }
            State::Loading if self.commits.is_empty() => {
                empty_state("Reading commits…", None).into_any_element()
            }
            _ => {
                let list = self.render_list(cx);
                let detail = match &self.detail {
                    Some(detail) => detail.clone().into_any_element(),
                    None => {
                        empty_state("Select a commit to see its changes.", None).into_any_element()
                    }
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
                            .border_color(border)
                            .child(detail),
                    )
                    .into_any_element()
            }
        };
        v_flex()
            .key_context("History")
            .track_focus(&self.focus_handle)
            .size_full()
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::confirm))
            .child(header)
            .children(warning)
            .child(body)
    }
}
