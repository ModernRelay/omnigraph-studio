//! One commit's changes against its first parent (spec 009), with the same entity cards as the
//! review. Shown in History's detail split, or as its own tab through `OpenCommit`.

use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, ListAlignment,
    ListState, Render, SharedString, Task, Window, list,
};
use gq_language::SchemaModel;
use graph_client::ReadError;
use graph_client::api::{ChangeCauseOutput, CommitOutput};
use graph_store::{
    ChangeFold, FoldedChange, FoldedChanges, GraphSession, NetChange, TypeCounts, summary_sentence,
};
use studio_actions::graph::OpenCommit;
use studio_ui::{
    ChangeCard, ChangeKind, CopyableValue, PropertyRow, change_counts, change_group_header,
    empty_state, now_micros, relative_time, short_commit_id, type_swatch, utc_date_time,
};
use studio_workspace::{Item, ItemEvent};
use ui::prelude::*;

/// Changes asked for per page.
const PAGE: u32 = 500;
/// Changes read before asking the person to load more (constitution IX).
const CAP: usize = 5_000;

/// A commit's counts, once its changes are read; History shows them in its list.
#[derive(Debug, Clone)]
pub struct CommitCounts {
    pub commit_id: String,
    pub added: usize,
    pub changed: usize,
    pub removed: usize,
}

#[derive(Debug, Clone, Copy)]
enum Row {
    Group(usize),
    Card(usize),
}

enum State {
    Loading,
    Ready,
    Failed(SharedString),
}

pub struct CommitView {
    session: Entity<GraphSession>,
    commit_id: String,
    commit: Option<CommitOutput>,
    cause: Option<ChangeCauseOutput>,
    state: State,
    fold: ChangeFold,
    folded: FoldedChanges,
    counts: Vec<TypeCounts>,
    rows: Vec<Row>,
    next_page_token: Option<String>,
    list_state: ListState,
    focus_handle: FocusHandle,
    _task: Option<Task<()>>,
}

impl EventEmitter<ItemEvent> for CommitView {}
impl EventEmitter<CommitCounts> for CommitView {}

impl Focusable for CommitView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for CommitView {
    fn tab_title(&self, _cx: &App) -> SharedString {
        format!("Commit {}", short_commit_id(&self.commit_id)).into()
    }

    fn tab_icon(&self, _cx: &App) -> Option<IconName> {
        Some(IconName::GitCommit)
    }

    fn dedupe_key(&self, _cx: &App) -> Option<SharedString> {
        Some(commit_key(&self.commit_id).into())
    }
}

pub fn commit_key(commit_id: &str) -> String {
    format!("commit:{commit_id}")
}

impl CommitView {
    pub fn new(
        session: Entity<GraphSession>,
        commit_id: String,
        commit: Option<CommitOutput>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self {
            session,
            commit_id,
            commit,
            cause: None,
            state: State::Loading,
            fold: ChangeFold::new(),
            folded: FoldedChanges::default(),
            counts: Vec::new(),
            rows: Vec::new(),
            next_page_token: None,
            list_state: ListState::new(0, ListAlignment::Top, px(600.)),
            focus_handle: cx.focus_handle(),
            _task: None,
        };
        view.load_more(cx);
        view
    }

    pub fn commit_id(&self) -> &str {
        &self.commit_id
    }

    /// Reads the next pages of changes, up to [`CAP`] more.
    fn load_more(&mut self, cx: &mut Context<Self>) {
        self.state = State::Loading;
        let client = self.session.read(cx).client(cx);
        let graph = self.session.read(cx).graph_id.clone();
        let commit_id = self.commit_id.clone();
        let mut token = self.next_page_token.take();
        let mut fold = self.fold.clone();
        let start = fold.raw_changes();
        self._task = Some(cx.spawn(async move |this, cx| {
            let mut cause = None;
            let result = loop {
                match client
                    .commit_changes(&graph, &commit_id, token.as_deref(), PAGE)
                    .await
                {
                    Ok(page) => {
                        let block = graph_client::api::ChangeBlockOutput {
                            cause: page.cause.clone(),
                            changes: page.changes,
                        };
                        cause = Some(page.cause);
                        fold = cx
                            .background_executor()
                            .spawn(async move {
                                fold.add_block(block);
                                fold
                            })
                            .await;
                        token = page.next_page_token;
                        if token.is_none() || fold.raw_changes() - start >= CAP {
                            break Ok(());
                        }
                    }
                    Err(error) => break Err(describe(&error)),
                }
            };
            let folded = fold.clone().finish();
            this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        this.cause = cause.or(this.cause.take());
                        this.fold = fold;
                        this.counts = folded.counts();
                        this.rows = build_rows(&folded);
                        this.list_state.reset(this.rows.len());
                        this.folded = folded;
                        this.next_page_token = token;
                        this.state = State::Ready;
                        let (added, changed, removed) =
                            this.counts.iter().fold((0, 0, 0), |(a, c, r), t| {
                                (a + t.added, c + t.changed, r + t.removed)
                            });
                        cx.emit(CommitCounts {
                            commit_id: this.commit_id.clone(),
                            added,
                            changed,
                            removed,
                        });
                    }
                    Err(message) => this.state = State::Failed(message.into()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn render_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let border = cx.theme().colors().border_variant;
        let actor = self
            .commit
            .as_ref()
            .and_then(|c| c.actor_id.clone())
            .or_else(|| self.cause.as_ref().and_then(|c| c.actor_id.clone()));
        let at = self
            .commit
            .as_ref()
            .map(|c| c.created_at)
            .or_else(|| self.cause.as_ref().map(|c| c.authored_at));
        // A listed commit without an authored branch was written on main.
        let branch = self
            .cause
            .as_ref()
            .map(|c| c.authored_branch.clone())
            .or_else(|| {
                self.commit
                    .as_ref()
                    .map(|c| c.graph_branch.clone().unwrap_or_else(|| "main".into()))
            });
        let parent = self
            .commit
            .as_ref()
            .and_then(|c| c.parent_commit_id.clone())
            .or_else(|| self.cause.as_ref().and_then(|c| c.parent_commit_id.clone()));
        let merged = self
            .commit
            .as_ref()
            .and_then(|c| c.merged_parent_commit_id.clone())
            .or_else(|| {
                self.cause
                    .as_ref()
                    .and_then(|c| c.merged_parent_commit_id.clone())
            });
        let sentence = match &self.state {
            State::Ready => {
                let actors: Vec<String> = actor.iter().cloned().collect();
                let mut sentence = summary_sentence(
                    self.folded.entity_count(),
                    self.folded.relationship_count(),
                    1,
                    &actors,
                );
                if self.next_page_token.is_some() {
                    sentence.push_str(&format!(" (first {} changes)", self.fold.raw_changes()));
                }
                sentence
            }
            State::Loading => "Reading changes…".into(),
            State::Failed(_) => String::new(),
        };
        let meta = |label: &'static str, value: AnyElement| {
            h_flex()
                .gap_2()
                .child(
                    div().w(px(64.)).flex_none().child(
                        Label::new(label)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
                )
                .child(value)
        };
        let parent_link = |id: Option<String>, name: &'static str| -> AnyElement {
            match id {
                Some(id) => Button::new(name, short_commit_id(&id))
                    .label_size(LabelSize::Small)
                    .style(ButtonStyle::Subtle)
                    .on_click(move |_, window, cx| {
                        window.dispatch_action(
                            Box::new(OpenCommit {
                                commit_id: id.clone(),
                            }),
                            cx,
                        )
                    })
                    .into_any_element(),
                None => Label::new("none")
                    .size(LabelSize::Small)
                    .color(Color::Disabled)
                    .into_any_element(),
            }
        };
        v_flex()
            .flex_none()
            .px_4()
            .py_3()
            .gap_1()
            .border_b_1()
            .border_color(border)
            .child(
                h_flex()
                    .gap_2()
                    .child(Icon::new(IconName::GitCommit).color(Color::Muted))
                    .child(
                        Headline::new(format!("Commit {}", short_commit_id(&self.commit_id)))
                            .size(HeadlineSize::XSmall),
                    ),
            )
            .child(meta(
                "Id",
                CopyableValue::new("commit-id", Some(self.commit_id.clone())).into_any_element(),
            ))
            .child(meta(
                "Actor",
                CopyableValue::new("actor", actor)
                    .plain()
                    .into_any_element(),
            ))
            .child(meta(
                "When",
                Label::new(match at {
                    Some(at) => format!(
                        "{} · {}",
                        relative_time(at, now_micros()),
                        utc_date_time(at)
                    ),
                    None => "—".into(),
                })
                .size(LabelSize::Small)
                .into_any_element(),
            ))
            .child(meta(
                "Branch",
                Label::new(branch.unwrap_or_else(|| "—".into()))
                    .size(LabelSize::Small)
                    .into_any_element(),
            ))
            .child(meta(
                "Parents",
                h_flex()
                    .gap_1()
                    .child(parent_link(parent, "parent"))
                    .when(merged.is_some(), |d| {
                        d.child(
                            Label::new("merged")
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(parent_link(merged, "merged-parent"))
                    })
                    .into_any_element(),
            ))
            .when(!sentence.is_empty(), |d| {
                d.child(div().pt_1().child(Label::new(sentence)))
            })
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_x_4()
                    .children(self.counts.iter().map(|count| {
                        h_flex()
                            .gap_1p5()
                            .child(type_swatch(&count.type_name))
                            .child(Label::new(count.type_name.clone()).size(LabelSize::Small))
                            .child(change_counts(count.added, count.changed, count.removed))
                    })),
            )
            .into_any_element()
    }

    fn render_row(
        &mut self,
        row: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let more = self.next_page_token.is_some();
        match self.rows.get(row).copied() {
            Some(Row::Group(ix)) => {
                let count = &self.counts[ix];
                div()
                    .px_4()
                    .child(change_group_header(
                        &count.type_name,
                        count.is_edge,
                        count.added,
                        count.changed,
                        count.removed,
                    ))
                    .into_any_element()
            }
            Some(Row::Card(ix)) => {
                let schema = self.session.read(cx).schema_model();
                let change = &self.folded.changes[ix];
                div()
                    .px_4()
                    .py_1()
                    .child(change_card(
                        change,
                        schema.as_deref(),
                        ("commit-change", ix),
                    ))
                    .into_any_element()
            }
            None if more => div()
                .px_4()
                .py_2()
                .child(
                    Button::new("load-more", "Load more changes")
                        .style(ButtonStyle::Subtle)
                        .on_click(cx.listener(|this, _, _, cx| this.load_more(cx))),
                )
                .into_any_element(),
            None => div().into_any_element(),
        }
    }
}

fn describe(error: &ReadError) -> String {
    match error {
        ReadError::Refused(summary) if summary.status == 409 => format!(
            "This commit cannot be compared with a parent (the first commit, or a schema change): {}",
            summary.message
        ),
        ReadError::Refused(summary) if summary.status == 410 => {
            "The history behind this commit was cleaned up, so its changes cannot be read.".into()
        }
        other => other.to_string(),
    }
}

/// The card for one folded change.
pub(crate) fn change_card(
    change: &FoldedChange,
    schema: Option<&SchemaModel>,
    id: impl Into<gpui::ElementId>,
) -> ChangeCard {
    let kind = match change.net() {
        Some(NetChange::Added) => ChangeKind::Added,
        Some(NetChange::Removed) => ChangeKind::Removed,
        _ => ChangeKind::Changed,
    };
    let rows = change
        .card_diffs(schema)
        .into_iter()
        .map(|diff| PropertyRow {
            name: diff.name.into(),
            before: diff.before,
            after: diff.after,
        })
        .collect();
    let mut card =
        ChangeCard::new(id, kind, change.type_name.clone(), change.id.clone()).rows(rows);
    if let Some((from, to)) = change.endpoints() {
        card = card.endpoints(from, to);
    }
    card
}

fn build_rows(folded: &FoldedChanges) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut group = None;
    let mut group_index = 0;
    for (ix, change) in folded.changes.iter().enumerate() {
        let key = (change.is_edge(), change.type_id.as_str());
        if group != Some(key) {
            if group.is_some() {
                group_index += 1;
            }
            group = Some(key);
            rows.push(Row::Group(group_index));
        }
        rows.push(Row::Card(ix));
    }
    rows
}

impl Render for CommitView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let header = self.render_header(cx);
        let more = usize::from(self.next_page_token.is_some());
        let body = match &self.state {
            State::Failed(message) => {
                empty_state("The changes could not be read.", Some(message.clone()))
                    .into_any_element()
            }
            State::Ready if self.rows.is_empty() => {
                empty_state("This commit changed no entities.", None).into_any_element()
            }
            State::Loading if self.rows.is_empty() => {
                empty_state("Reading changes…", None).into_any_element()
            }
            _ => {
                if self.list_state.item_count() != self.rows.len() + more {
                    self.list_state.reset(self.rows.len() + more);
                }
                list(
                    self.list_state.clone(),
                    cx.processor(|this, row, window, cx| this.render_row(row, window, cx)),
                )
                .flex_1()
                .min_h_0()
                .py_2()
                .into_any_element()
            }
        };
        v_flex()
            .key_context("CommitView")
            .track_focus(&self.focus_handle)
            .size_full()
            .child(header)
            .child(body)
    }
}
