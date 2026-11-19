//! Comparing two branches, or a branch and a commit (spec 043): the dialog that picks what a
//! branch is compared with. The comparison itself is a review tab in its read-only mode
//! ([`crate::view::ReviewView::compare`]).

use std::ops::Range;

use editor::{Editor, EditorEvent};
use gpui::{
    AnyElement, App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render,
    SharedString, Subscription, UniformListScrollHandle, Window, uniform_list,
};
use graph_store::{GraphSession, SessionEvent};
use studio_actions::graph::OpenCompare;
use studio_ui::ds;
use studio_ui::ds::Tipped;
use studio_workspace::ModalView;
use ui::prelude::*;

pub use crate::view::open_compare;

/// The CLI command that reads the same differences.
pub(crate) fn compare_cli(from: &str, to: &str) -> String {
    format!("omnigraph diff --from {from} --to {to}")
}

/// How many rows of branches the dialog shows at once; the list scrolls past them.
const ROWS_SHOWN: usize = 8;

/// "Compare <branch> with…": a field that narrows the graph's branches, and the branch picked,
/// or a commit id typed, is compared with the first. Nothing is written.
pub struct CompareModal {
    session: Entity<GraphSession>,
    from: String,
    query: Entity<Editor>,
    /// The branches the query leaves, in the list's order, without `from`.
    matches: Vec<String>,
    selected: Option<usize>,
    scroll: UniformListScrollHandle,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DismissEvent> for CompareModal {}

impl ModalView for CompareModal {}

impl Focusable for CompareModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.query.focus_handle(cx)
    }
}

impl CompareModal {
    pub fn new(
        session: Entity<GraphSession>,
        from: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let query = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("A branch, or a commit id", window, cx);
            editor
        });
        let subscriptions = vec![
            cx.subscribe(&query, |this, _, event: &EditorEvent, cx| {
                if let EditorEvent::BufferEdited = event {
                    this.narrow(cx);
                }
            }),
            // A branch list read while the dialog is open (a Refresh, the poll) is offered.
            cx.subscribe(&session, |this, _, event: &SessionEvent, cx| {
                if let SessionEvent::BranchesChanged = event {
                    this.narrow(cx);
                }
            }),
        ];
        let mut this = Self {
            session,
            from,
            query,
            matches: Vec::new(),
            selected: None,
            scroll: UniformListScrollHandle::new(),
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
        };
        this.narrow(cx);
        this
    }

    fn query_text(&self, cx: &App) -> String {
        self.query.read(cx).text(cx).trim().to_owned()
    }

    /// The branches whose name holds every word typed, case-blind; the first is selected.
    fn narrow(&mut self, cx: &mut Context<Self>) {
        let query = self.query_text(cx).to_lowercase();
        let words: Vec<&str> = query.split_whitespace().collect();
        let matches: Vec<String> = self
            .session
            .read(cx)
            .branches
            .loaded()
            .map(|branches| {
                branches
                    .iter()
                    .filter(|branch| **branch != self.from)
                    .filter(|branch| {
                        let name = branch.to_lowercase();
                        words.iter().all(|word| name.contains(word))
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        if matches != self.matches {
            self.selected = (!matches.is_empty()).then_some(0);
            self.matches = matches;
            self.scroll.scroll_to_item(0, gpui::ScrollStrategy::Top);
        }
        cx.notify();
    }

    /// What the dialog would compare with now: the branch selected, else the text typed, read
    /// as a commit id.
    fn picked(&self, cx: &App) -> Option<String> {
        match self.selected.and_then(|ix| self.matches.get(ix)) {
            Some(branch) => Some(branch.clone()),
            None => Some(self.query_text(cx)).filter(|text| !text.is_empty() && *text != self.from),
        }
    }

    fn step(&mut self, forward: bool, cx: &mut Context<Self>) {
        if self.matches.is_empty() {
            return;
        }
        let last = self.matches.len() - 1;
        let next = match self.selected {
            Some(at) if forward => (at + 1).min(last),
            Some(at) => at.saturating_sub(1),
            None => 0,
        };
        self.selected = Some(next);
        self.scroll
            .scroll_to_item(next, gpui::ScrollStrategy::Nearest);
        cx.notify();
    }

    fn compare(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(to) = self.picked(cx) else {
            return;
        };
        window.dispatch_action(
            Box::new(OpenCompare {
                from: self.from.clone(),
                to,
                preview: false,
            }),
            cx,
        );
        cx.emit(DismissEvent);
    }

    fn render_rows(&mut self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        range
            .filter_map(|ix| {
                let branch = self.matches.get(ix)?.clone();
                let row = ds::SidebarRow::new(("compare-branch", ix), branch)
                    .icon(IconName::GitBranch)
                    .selected(self.selected == Some(ix))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.selected = Some(ix);
                        this.compare(window, cx);
                    }));
                // As wide as the list, so the selected row's pill spans it.
                Some(div().w_full().child(row).into_any_element())
            })
            .collect()
    }
}

impl Render for CompareModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let picked = self.picked(cx);
        let typed_commit = self.selected.is_none() && picked.is_some();
        let cli = compare_cli(&self.from, picked.as_deref().unwrap_or("<branch>"));
        let list: AnyElement = if self.matches.is_empty() {
            ds::sidebar_note(match (&picked, self.session.read(cx).branches.loaded()) {
                (_, None) => "Reading branches…".to_owned(),
                (Some(text), _) => format!("No branch is named {text}: it is read as a commit id."),
                (None, _) => "No other branch.".to_owned(),
            })
            .into_any_element()
        } else {
            let rows = self.matches.len().min(ROWS_SHOWN);
            uniform_list(
                "compare-branches",
                self.matches.len(),
                cx.processor(|this, range: Range<usize>, _, cx| this.render_rows(range, cx)),
            )
            .track_scroll(&self.scroll)
            .h(px(ds::size::SIDEBAR_ROW * rows as f32))
            .into_any_element()
        };
        let description: SharedString = format!(
            "What the other side holds that {} does not, as a review's cards. Nothing is written.",
            self.from
        )
        .into();
        ds::modal(cx)
            .key_context("CompareModal")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| this.compare(window, cx)))
            .on_action(cx.listener(|this, _: &menu::SelectNext, _, cx| this.step(true, cx)))
            .on_action(cx.listener(|this, _: &menu::SelectPrevious, _, cx| this.step(false, cx)))
            .child(
                ds::ModalHeader::new(format!("Compare {} with…", self.from))
                    .description(description)
                    .on_close(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
            )
            .child(
                ds::modal_body()
                    .child(ds::field_frame(false, cx).child(ds::field_input(self.query.clone())))
                    .child(list),
            )
            .child(
                ds::modal_footer(cx)
                    .child(
                        ds::cancel_button("cancel")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        ds::action_button(
                            "compare",
                            if typed_commit {
                                "Compare with commit"
                            } else {
                                "Compare"
                            },
                        )
                        .disabled(picked.is_none())
                        .tip_fn(move |_, cx| {
                            ds::tooltip_with_meta("Open the comparison", None, cli.clone(), cx)
                        })
                        .on_click(cx.listener(|this, _, window, cx| this.compare(window, cx))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::view::tests::fake_server;

    /// The dialog narrows the other branches by every word typed, ↓ and ↑ move through them,
    /// and text that names no branch is compared as a commit id; the branch compared from is
    /// never offered.
    #[gpui::test]
    async fn the_dialog_narrows_the_branches_and_reads_other_text_as_a_commit(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let (session, _) = fake_server(
            |path, _, _| {
                if path.ends_with("/branches") {
                    (
                        200,
                        json!({ "branches": ["main", "agent/notes-oct-14", "agent/slack-oct-14", "plan/q4"] }),
                    )
                } else {
                    (404, json!({ "error": "not here" }))
                }
            },
            cx,
        );
        session.update(cx, |session, cx| session.reload_branches(cx));
        cx.run_until_parked();
        let (modal, cx) = cx.add_window_view(|window, cx| {
            CompareModal::new(session, "agent/notes-oct-14".into(), window, cx)
        });
        cx.run_until_parked();
        let typed = |text: &str, cx: &mut gpui::VisualTestContext| {
            let text = text.to_owned();
            modal.update_in(cx, |modal, window, cx| {
                modal
                    .query
                    .update(cx, |editor, cx| editor.set_text(text, window, cx))
            });
            cx.run_until_parked();
        };
        modal.read_with(cx, |modal, cx| {
            assert_eq!(modal.matches, ["main", "agent/slack-oct-14", "plan/q4"]);
            assert_eq!(modal.picked(cx).as_deref(), Some("main"));
        });
        modal.update(cx, |modal, cx| modal.step(true, cx));
        modal.read_with(cx, |modal, cx| {
            assert_eq!(modal.picked(cx).as_deref(), Some("agent/slack-oct-14"))
        });
        typed("SLACK oct", cx);
        modal.read_with(cx, |modal, cx| {
            assert_eq!(modal.matches, ["agent/slack-oct-14"]);
            assert_eq!(modal.picked(cx).as_deref(), Some("agent/slack-oct-14"));
        });
        typed("69T7SP95", cx);
        modal.read_with(cx, |modal, cx| {
            assert!(modal.matches.is_empty());
            assert_eq!(modal.picked(cx).as_deref(), Some("69T7SP95"), "a commit id");
        });
        typed("agent/notes-oct-14", cx);
        modal.read_with(cx, |modal, cx| {
            assert_eq!(modal.picked(cx), None, "not compared with itself")
        });
    }
}
