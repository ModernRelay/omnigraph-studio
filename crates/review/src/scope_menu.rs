//! The review's scope menu (spec 008): everything since the fork, or one commit of the branch.
//!
//! A branch can hold thousands of commits since its fork, so they are a list that draws, and
//! labels, only the rows in view (constitution IX), while the keys move through every row.

use std::ops::Range;
use std::sync::Arc;

use gpui::{
    AnyElement, App, Context, DismissEvent, EventEmitter, FocusHandle, Focusable, MouseDownEvent,
    Render, Role, ScrollStrategy, UniformListScrollHandle, WeakEntity, Window, uniform_list,
};
use studio_ui::ds::{size, space};
use studio_ui::menu::{MenuRow, menu_header, menu_separator, menu_surface};
use studio_ui::{now_micros, relative_time, short_commit_id};
use ui::prelude::*;

use crate::view::{ReviewCommit, ReviewView};

/// Commit rows the menu shows at once; the rest scroll.
const SHOWN_ROWS: usize = 12;

/// "Since the fork", then the branch's commits since the fork, newest first: choosing one
/// reviews it.
pub(crate) struct ScopeMenu {
    view: WeakEntity<ReviewView>,
    /// The review's own list, shared: nothing is copied or labelled as the menu opens.
    commits: Arc<[ReviewCommit]>,
    /// The highlighted row: 0 is "Since the fork", `n` the `n`th commit.
    selected: usize,
    /// When the menu opened, which each commit's age is told from.
    now: i64,
    /// The commit whose row is measured for the list's width: the one with the longest actor,
    /// found from lengths, without making every label.
    widest: usize,
    scroll: UniformListScrollHandle,
    focus_handle: FocusHandle,
    /// How many commit rows the list has built, for the test that it builds only those in view.
    #[cfg(test)]
    rows_drawn: usize,
}

impl EventEmitter<DismissEvent> for ScopeMenu {}

impl Focusable for ScopeMenu {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ScopeMenu {
    /// The menu for `commits`, the review showing `scope` (`None` for everything since the
    /// fork), which it highlights and scrolls to.
    pub(crate) fn new(
        view: WeakEntity<ReviewView>,
        commits: Arc<[ReviewCommit]>,
        scope: Option<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        let selected = scope
            .and_then(|id| commits.iter().position(|commit| commit.id == id))
            .map_or(0, |at| at + 1);
        let widest = commits
            .iter()
            .enumerate()
            .max_by_key(|(_, commit)| commit.actor_id.as_ref().map_or(0, String::len))
            .map_or(0, |(at, _)| at);
        let scroll = UniformListScrollHandle::new();
        if let Some(at) = selected.checked_sub(1) {
            scroll.scroll_to_item(at, ScrollStrategy::Center);
        }
        Self {
            view,
            commits,
            selected,
            now: now_micros(),
            widest,
            scroll,
            focus_handle: cx.focus_handle(),
            #[cfg(test)]
            rows_drawn: 0,
        }
    }

    /// Rows the keys move through: "Since the fork", then each commit.
    fn rows(&self) -> usize {
        self.commits.len() + 1
    }

    fn select(&mut self, row: usize, cx: &mut Context<Self>) {
        self.selected = row.min(self.rows() - 1);
        if let Some(at) = self.selected.checked_sub(1) {
            self.scroll.scroll_to_item(at, ScrollStrategy::Nearest);
        }
        cx.notify();
    }

    fn select_next(&mut self, _: &menu::SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.select((self.selected + 1) % self.rows(), cx);
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let rows = self.rows();
        self.select((self.selected + rows - 1) % rows, cx);
    }

    fn select_first(&mut self, _: &menu::SelectFirst, _: &mut Window, cx: &mut Context<Self>) {
        self.select(0, cx);
    }

    fn select_last(&mut self, _: &menu::SelectLast, _: &mut Window, cx: &mut Context<Self>) {
        self.select(self.rows() - 1, cx);
    }

    fn confirm(&mut self, _: &menu::Confirm, _: &mut Window, cx: &mut Context<Self>) {
        self.choose(self.selected, cx);
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    /// Reviews the scope of `row`, then closes.
    fn choose(&mut self, row: usize, cx: &mut Context<Self>) {
        let scope = match row.checked_sub(1) {
            None => Some(None),
            Some(at) => self.commits.get(at).map(|commit| Some(commit.id.clone())),
        };
        if let Some(scope) = scope {
            self.view
                .update(cx, |view, cx| view.set_scope(scope, cx))
                .ok();
        }
        cx.emit(DismissEvent);
    }

    /// A commit's label: its short id, who made it and how long ago.
    fn label(&self, commit: &ReviewCommit) -> String {
        let mut label = short_commit_id(&commit.id);
        if let Some(actor) = &commit.actor_id {
            label.push_str(&format!(" · {actor}"));
        }
        label.push_str(&format!(
            " · {}",
            relative_time(commit.created_at, self.now)
        ));
        label
    }

    /// The commit rows in `range`, the only ones labelled.
    fn render_commits(&mut self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        #[cfg(test)]
        {
            self.rows_drawn += range.len();
        }
        range
            .filter_map(|at| {
                let label = self.label(self.commits.get(at)?);
                Some(
                    MenuRow::new(("scope-commit", at))
                        .aria_role(Role::MenuItem)
                        .aria_label(label.clone())
                        .toggle_state(self.selected == at + 1)
                        .on_click(cx.listener(move |this, _, _, cx| this.choose(at + 1, cx)))
                        .child(studio_ui::ds::text::plain(label).truncate())
                        .into_any_element(),
                )
            })
            .collect()
    }
}

impl Render for ScopeMenu {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.commits.len();
        let since = MenuRow::new("scope-since")
            .aria_role(Role::MenuItem)
            .aria_label("Since the fork")
            .toggle_state(self.selected == 0)
            .on_click(cx.listener(|this, _, _, cx| this.choose(0, cx)))
            .child(studio_ui::ds::text::plain("Since the fork"));
        let commits = (count > 0).then(|| {
            uniform_list(
                "review-scope-commits",
                count,
                cx.processor(|this, range: Range<usize>, _window, cx| {
                    this.render_commits(range, cx)
                }),
            )
            .with_width_from_item(Some(self.widest))
            .track_scroll(&self.scroll)
            .h(px(size::MENU_ROW * count.min(SHOWN_ROWS) as f32))
        });
        menu_surface(v_flex(), cx)
            .id("review-scope-menu")
            .role(Role::Menu)
            .key_context("menu")
            .track_focus(&self.focus_handle)
            .occlude()
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .on_mouse_down_out(cx.listener(|_, _: &MouseDownEvent, _, cx| cx.emit(DismissEvent)))
            .min_w(px(size::MENU_MIN_WIDTH))
            .p(px(space::XS))
            .child(since)
            .when_some(commits, |menu, commits| {
                menu.child(menu_separator(cx))
                    .child(menu_header("One commit".into()))
                    .child(commits)
            })
    }
}

#[cfg(test)]
mod tests {
    use gpui::Entity;

    use super::*;
    use crate::view::tests::reviewed_server;

    fn commits(n: usize) -> Arc<[ReviewCommit]> {
        (0..n)
            .rev()
            .map(|i| ReviewCommit {
                id: format!("c{i:05}"),
                actor_id: Some("act-dev".into()),
                created_at: 0,
            })
            .collect()
    }

    /// The menu draws a screenful of a branch's 10,000 commits since its fork, not one row per
    /// commit at every frame, and the keys still reach every row: down past the last returns
    /// to "Since the fork", up from it goes to the oldest commit.
    #[gpui::test]
    async fn the_scope_menu_builds_only_the_commits_in_view(cx: &mut gpui::TestAppContext) {
        cx.update(studio_theme::init_test);
        let (menu, cx) = cx.add_window_view(|_, cx| {
            ScopeMenu::new(
                WeakEntity::new_invalid(),
                commits(10_000),
                Some("c09990".into()),
                cx,
            )
        });
        cx.run_until_parked();
        menu.update(cx, |menu, cx| {
            assert_eq!(menu.selected, 10, "the scope shown");
            menu.rows_drawn = 0;
            cx.notify();
        });
        cx.run_until_parked();
        let drawn = menu.read_with(cx, |menu, _| menu.rows_drawn);
        assert!(drawn > 0, "the rows in view are drawn");
        assert!(drawn < 100, "{drawn} rows built of 10,000");

        cx.update(|window, cx| window.focus(&menu.focus_handle(cx), cx));
        cx.dispatch_action(menu::SelectLast);
        menu.read_with(cx, |menu, _| assert_eq!(menu.selected, 10_000));
        cx.dispatch_action(menu::SelectNext);
        menu.read_with(cx, |menu, _| assert_eq!(menu.selected, 0));
        cx.dispatch_action(menu::SelectPrevious);
        menu.read_with(cx, |menu, _| assert_eq!(menu.selected, 10_000));
        cx.dispatch_action(menu::SelectFirst);
        cx.dispatch_action(menu::SelectNext);
        menu.read_with(cx, |menu, _| assert_eq!(menu.selected, 1));
        cx.run_until_parked();
        let drawn = menu.read_with(cx, |menu, _| menu.rows_drawn);
        assert!(
            drawn < 400,
            "{drawn} rows built while moving through 10,000"
        );
    }

    /// ↵ reviews the highlighted commit and closes the menu, which opens again on it; choosing
    /// "Since the fork" goes back to every commit.
    #[gpui::test]
    async fn the_scope_menu_reviews_the_commit_chosen(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let (session, _) = reviewed_server(false, cx);
        let (view, cx) = cx.add_window_view(|window, cx| {
            ReviewView::new(session, "feature".into(), "main".into(), window, cx)
        });
        cx.run_until_parked();
        let closed = std::rc::Rc::new(std::cell::Cell::new(0));
        let open = |cx: &mut gpui::VisualTestContext| -> Entity<ScopeMenu> {
            let (commits, scope) = view.read_with(cx, |view, _| view.scope_menu_input());
            let weak = view.downgrade();
            let menu = cx.new(|cx| ScopeMenu::new(weak, commits, scope, cx));
            let seen = closed.clone();
            cx.update(|_, cx| {
                cx.subscribe(&menu, move |_, _: &DismissEvent, _| {
                    seen.set(seen.get() + 1)
                })
                .detach()
            });
            menu
        };
        let menu = open(cx);
        menu.read_with(cx, |menu, _| assert_eq!(menu.selected, 0, "since the fork"));
        // Newest first: f2, then f1.
        menu.update_in(cx, |menu, window, cx| {
            menu.select_next(&menu::SelectNext, window, cx);
            menu.select_next(&menu::SelectNext, window, cx);
            menu.confirm(&menu::Confirm, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(closed.get(), 1);
        view.read_with(cx, |view, _| {
            assert_eq!(view.scope_menu_input().1.as_deref(), Some("f1"))
        });
        let menu = open(cx);
        menu.read_with(cx, |menu, _| {
            assert_eq!(menu.selected, 2, "the scope shown")
        });
        menu.update(cx, |menu, cx| menu.choose(0, cx));
        cx.run_until_parked();
        assert_eq!(closed.get(), 2);
        view.read_with(cx, |view, _| assert_eq!(view.scope_menu_input().1, None));
    }
}
