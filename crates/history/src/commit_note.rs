//! A commit as the inspector shows it (spec 020) while its row is selected in History: what
//! the row says in short, said in full, with links to its parents. Its changes are the
//! commit's own tab, which a click on the row, or Changes here, opens.

use gpui::{
    ClickEvent, ClipboardItem, Context, IntoElement, Render, SharedString, Window, div, px,
};
use graph_client::api::CommitOutput;
use studio_actions::graph::OpenCommit;
use studio_ui::ds::{self, Tipped as _, size, space};
use studio_ui::{now_micros, relative_time, short_commit_id, utc_date_time};
use ui::prelude::*;

use crate::commit_view::{branch_fact, is_first_commit};
use crate::counts::Learnt;

/// A fact this short sits in a value box as a property's value does; a longer one (a held
/// commit's place) is read as text.
const BOXED: usize = 36;

pub(crate) struct CommitNote {
    commit: CommitOutput,
    /// The branches whose head the commit is.
    heads: Vec<String>,
    /// Its changes counted, once History has read them, or why there are none.
    counts: Option<Learnt>,
    /// What it was, in a few words, when History knows (spec 040).
    subject: Option<String>,
}

impl CommitNote {
    pub(crate) fn new(
        commit: CommitOutput,
        heads: Vec<String>,
        counts: Option<Learnt>,
        subject: Option<String>,
    ) -> Self {
        Self {
            commit,
            heads,
            counts,
            subject,
        }
    }

    /// Takes what History has learnt of the commit since: its counts, read once its row was
    /// drawn, and the branches at it. Says whether anything changed.
    pub(crate) fn learn(
        &mut self,
        heads: &[String],
        counts: Option<Learnt>,
        subject: Option<String>,
    ) -> bool {
        if self.heads == heads && self.counts == counts && self.subject == subject {
            return false;
        }
        self.heads = heads.to_vec();
        self.counts = counts;
        self.subject = subject;
        true
    }

    /// The commit's id, and what the note says of its changes, for a test.
    #[cfg(test)]
    pub(crate) fn read(&self) -> (&str, &[String], Option<(crate::counts::Counts, bool)>) {
        (
            &self.commit.graph_commit_id,
            &self.heads,
            self.counts.as_ref().and_then(Learnt::totals),
        )
    }
}

/// A link to a commit, by its short id: it opens the commit's tab in the center (spec 020), as
/// a preview, which a second click keeps.
fn commit_link(name: &'static str, id: &str, tooltip: &'static str, cx: &gpui::App) -> AnyElement {
    let open = {
        let id = id.to_owned();
        move |event: &ClickEvent, window: &mut Window, cx: &mut gpui::App| {
            window.dispatch_action(
                Box::new(OpenCommit {
                    commit_id: id.clone(),
                    preview: event.click_count() < 2,
                }),
                cx,
            )
        }
    };
    div()
        .id(name)
        .min_w_0()
        .cursor_pointer()
        .tip(ds::tooltip(tooltip))
        .on_click(open)
        .child(ds::value_text(short_commit_id(id), cx))
        .into_any_element()
}

impl Render for CommitNote {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let commit = &self.commit;
        let id = commit.graph_commit_id.clone();
        let fact = |icon: IconName, label: &'static str, value: AnyElement| {
            ds::property_row(icon, label, size::FIELD_LABEL)
                .child(ds::property_value().child(value))
        };
        // A short fact boxed, as a property's value; a long one as text that wraps.
        let said = |text: String, cx: &gpui::App| -> AnyElement {
            if text.chars().count() <= BOXED {
                ds::value_text(text, cx).into_any_element()
            } else {
                ds::text::body(text).into_any_element()
            }
        };
        let by = match &commit.actor_id {
            Some(actor) => said(actor.clone(), cx),
            None => ds::text::muted("—").into_any_element(),
        };
        let mut facts = vec![fact(IconName::Person, "By", by)];
        // What it was: Studio's own words, a merge's branch or the node it changed (spec 040).
        if let Some(subject) = &self.subject {
            facts.push(fact(IconName::Pencil, "What", said(subject.clone(), cx)));
        }
        facts.extend([
            // The row says how long ago; here, when. The two wrap where the panel is narrow.
            fact(
                IconName::Clock,
                "When",
                ds::value_list()
                    .items_center()
                    .child(ds::value_text(utc_date_time(commit.created_at), cx))
                    .child(ds::text::meta(relative_time(
                        commit.created_at,
                        now_micros(),
                    )))
                    .into_any_element(),
            ),
        ]);
        if let Some((icon, branch)) = branch_fact(Some(commit), None, false) {
            facts.push(fact(icon, "Branch", said(branch, cx)));
        }
        if let Some(parent) = &commit.parent_commit_id {
            facts.push(fact(
                IconName::GitCommit,
                "Parent",
                commit_link("commit-note-parent", parent, "Open the parent commit", cx),
            ));
        }
        if let Some(merged) = &commit.merged_parent_commit_id {
            facts.push(fact(
                IconName::PullRequest,
                "Merged",
                commit_link(
                    "commit-note-merged",
                    merged,
                    "Open the merged branch's commit",
                    cx,
                ),
            ));
        }
        if !self.heads.is_empty() {
            facts.push(fact(
                IconName::GitBranch,
                "Head of",
                ds::value_list()
                    .children(self.heads.iter().map(|name| ds::tag(name.clone(), cx)))
                    .into_any_element(),
            ));
        }
        // What it changed, as every list of commits says it, then by type (spec 033); the
        // first commit and one across a schema change say why there is nothing to count.
        let first = is_first_commit(commit).then(|| Learnt {
            counts: change_cards::CommitCounts::First,
            by_type: std::sync::Arc::from([]),
            touched: std::sync::Arc::from([]),
            touched_count: 0,
        });
        let changes: Option<AnyElement> = first.or_else(|| self.counts.clone()).map(|learnt| {
            v_flex()
                .gap(px(space::XS))
                .child(change_cards::commit_counts("note-counts", &learnt.counts))
                .when(!learnt.by_type.is_empty(), |value| {
                    value.child(change_cards::summary_line(&learnt.by_type, cx))
                })
                .into_any_element()
        });
        if let Some(changes) = changes {
            facts.push(fact(IconName::Pencil, "Changes", changes));
        }
        let (full, open) = (id.clone(), id.clone());
        v_flex()
            .px(px(space::LG))
            .pt(px(space::XL))
            .pb(px(space::LG))
            .gap(px(space::MD))
            .child(
                h_flex()
                    .gap(px(space::INLINE))
                    .child(ds::icon::meta(IconName::GitCommit))
                    .child(ds::text::meta("Commit"))
                    .child(div().flex_1())
                    .child(
                        ds::button("commit-note-open", "Changes")
                            .style(ButtonStyle::Subtle)
                            .tip(ds::tooltip("Open the commit's changes in a tab"))
                            .on_click(move |_, window, cx| {
                                window.dispatch_action(
                                    Box::new(OpenCommit {
                                        commit_id: open.clone(),
                                        preview: true,
                                    }),
                                    cx,
                                )
                            }),
                    ),
            )
            .child(
                div()
                    .id("commit-note-id")
                    .cursor_pointer()
                    .tip(ds::tooltip(SharedString::from(format!(
                        "{id}\nClick to copy"
                    ))))
                    .on_click(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(full.clone()))
                    })
                    .child(ds::text::heading(short_commit_id(&id)).buffer_font(cx)),
            )
            .child(ds::fields().children(facts))
    }
}
