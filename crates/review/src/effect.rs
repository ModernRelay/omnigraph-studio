//! A branch's Effect (spec 036): the reads a person marked for the graph's reviews, each run at
//! the target's head and at the branch's, the rows that came, went and moved, Studio's own
//! checks, and the merge's result held as an unpublished commit for the checks to run on.
//!
//! A tab of its own beside the review and the history of the branch (constitution XIII: it has
//! reads of its own). What it shows is the session's [`Effect`], which the review's merge strip
//! reads too.

use std::collections::HashSet;

use gpui::{
    AnyElement, AnyView, App, Context, Entity, EventEmitter, FocusHandle, Focusable, Render,
    SharedString, StyleRefinement, Subscription, WeakEntity, Window,
};
use graph_store::effect::{
    Effect, EffectRead, Heads, MergeResult, ResultStep, Rows, Side, Source, Want,
};
use graph_store::row_diff::RowDiff;
use graph_store::{GraphSession, Loadable};
use serde_json::Value;
use studio_actions::effect::{
    CheckMergeResult, DropMergeResult, NextRead, PickReads, PreviousRead, PublishMergeResult,
    ToggleRead,
};
use studio_actions::graph::{OpenBranch, OpenBranchHistory};
use studio_actions::workspace::Refresh;
use studio_actions::{ViewKind, ViewLink};
use studio_settings::{EffectMark, StudioSettings};
use studio_ui::ds::Tipped;
use studio_ui::ds::{size, space};
use studio_ui::menu::ContextMenu;
use studio_ui::short_commit_id;
use studio_workspace::{Item, ItemEvent, ItemStatus, KeyHint, SavedItem, Workspace};
use ui::prelude::*;
use ui::{KeyBinding, PopoverMenu, PopoverMenuHandle, Severity};

/// The most rows of each kind an unfolded read lists; the rest are counted.
pub(crate) const ROWS_SHOWN: usize = 50;
/// The most cells a listed row shows.
const CELLS_SHOWN: usize = 4;

/// The tab.
pub struct EffectView {
    session: Entity<GraphSession>,
    pub(crate) effect: Entity<Effect>,
    branch: String,
    target: String,
    focus_handle: FocusHandle,
    /// The read the keys are on, by its key.
    selected: Option<String>,
    /// The reads unfolded to their rows, by key.
    open: HashSet<String>,
    /// The list of reads, a cached view of its own (`docs/drawing.md`).
    list: Entity<EffectRows>,
    /// The menu of reads to run, which its key opens too.
    picker_menu: PopoverMenuHandle<ContextMenu>,
    /// The session asks after both branches' heads while the tab is open, so a commit to
    /// either offers Refresh (constitution II), whatever branch the window is on.
    _head_watches: [graph_store::HeadWatch; 2],
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ItemEvent> for EffectView {}

impl Focusable for EffectView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// The dedupe key of an Effect tab.
pub fn effect_key(branch: &str, target: &str) -> String {
    format!("effect:{branch}->{target}")
}

impl Item for EffectView {
    fn tab_title(&self, _cx: &App) -> SharedString {
        format!("Effect {} → {}", self.branch, self.target).into()
    }

    fn tab_icon(&self, _cx: &App) -> Option<IconName> {
        Some(IconName::CheckDouble)
    }

    fn thing(&self, _cx: &App) -> Option<studio_actions::Thing> {
        Some(studio_actions::Thing::Branch(self.branch.clone()))
    }

    fn views(&self, _cx: &App) -> Vec<ViewLink> {
        vec![
            ViewLink::to(ViewKind::Review),
            ViewLink::to(ViewKind::History),
            ViewLink::current(ViewKind::Effect),
        ]
    }

    fn view_action(&self, kind: ViewKind, _cx: &App) -> Option<Box<dyn gpui::Action>> {
        let branch = self.branch.clone();
        match kind {
            ViewKind::Review => Some(Box::new(OpenBranch {
                branch,
                preview: true,
            })),
            ViewKind::History => Some(Box::new(OpenBranchHistory {
                branch,
                preview: true,
            })),
            _ => None,
        }
    }

    fn dedupe_key(&self, _cx: &App) -> Option<SharedString> {
        Some(effect_key(&self.branch, &self.target).into())
    }

    fn serialize(&self, _cx: &App) -> Option<SavedItem> {
        Some(SavedItem {
            kind: "effect".into(),
            data: serde_json::json!({ "branch": self.branch, "target": self.target }),
        })
    }

    fn is_stale(&self, cx: &App) -> bool {
        self.effect.read(cx).moved
    }

    fn caches_its_parts(&self) -> bool {
        true
    }

    /// The bottom bar's fact (how many checks have rows at the branch head) and the tab's keys.
    fn status(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> Option<ItemStatus> {
        let effect = self.effect.read(cx);
        let failing = effect.checks().filter(|read| read.fails()).count();
        let fact = if failing > 0 {
            studio_ui::ds::meta(
                IconName::Warning,
                studio_ui::plural(failing, "check has rows", "checks have rows"),
            )
        } else {
            studio_ui::ds::meta(
                IconName::ListTodo,
                studio_ui::plural(effect.reads().len(), "read", "reads"),
            )
        };
        let focus = &self.focus_handle;
        Some(ItemStatus {
            start: vec![fact.into_any_element()],
            end: Vec::new(),
            keys: vec![
                KeyHint::new("Next read", &NextRead, focus),
                KeyHint::new("Previous read", &PreviousRead, focus),
                KeyHint::new("Show its rows", &ToggleRead, focus),
                KeyHint::new("Choose the reads", &PickReads, focus),
                KeyHint::new("Check the merge result", &CheckMergeResult, focus),
                KeyHint::new("Read again", &Refresh, focus),
            ],
        })
    }
}

impl EffectView {
    pub fn new(
        session: Entity<GraphSession>,
        branch: String,
        target: String,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let effect = session.update(cx, |session, cx| session.effect(&branch, &target, cx));
        let head_watches = session.update(cx, |session, cx| {
            [
                session.watch_head(&branch, cx),
                session.watch_head(&target, cx),
            ]
        });
        effect.update(cx, |effect, cx| effect.want(Want::All, None, cx));
        let list = EffectRows::new(cx);
        let subscriptions = vec![cx.observe(&effect, |this, _, cx| {
            this.keep_selection(cx);
            cx.emit(ItemEvent::UpdateTab);
            cx.notify();
        })];
        Self {
            session,
            effect,
            branch,
            target,
            focus_handle: cx.focus_handle(),
            selected: None,
            open: HashSet::new(),
            list,
            picker_menu: PopoverMenuHandle::default(),
            _head_watches: head_watches,
            _subscriptions: subscriptions,
        }
    }

    /// The selection stays on a read still listed, else moves to the first.
    fn keep_selection(&mut self, cx: &App) {
        let reads = self.effect.read(cx).reads();
        if self
            .selected
            .as_ref()
            .is_none_or(|key| !reads.iter().any(|read| &read.key == key))
        {
            self.selected = reads.first().map(|read| read.key.clone());
        }
    }

    fn select_step(&mut self, step: isize, cx: &mut Context<Self>) {
        let keys: Vec<String> = self
            .effect
            .read(cx)
            .reads()
            .iter()
            .map(|read| read.key.clone())
            .collect();
        if keys.is_empty() {
            return;
        }
        let at = self
            .selected
            .as_ref()
            .and_then(|key| keys.iter().position(|k| k == key));
        let next = match at {
            None => 0,
            Some(at) => (at as isize + step).clamp(0, keys.len() as isize - 1) as usize,
        };
        if at != Some(next) {
            self.selected = Some(keys[next].clone());
            cx.notify();
        }
    }

    fn toggle(&mut self, key: String, cx: &mut Context<Self>) {
        if !self.open.remove(&key) {
            self.open.insert(key.clone());
        }
        self.selected = Some(key);
        cx.notify();
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.effect.update(cx, |effect, cx| effect.refresh(cx));
    }

    fn graph_key(&self, cx: &App) -> String {
        self.session.read(cx).overrides_key()
    }

    /// The menu of stored reads a review may run: each opens to what it is to the graph's
    /// reviews, the one it is now ticked.
    fn picker(
        view: WeakEntity<Self>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Entity<ContextMenu>> {
        let this = view.upgrade()?;
        let (graph, session) = {
            let this = this.read(cx);
            (this.graph_key(cx), this.session.clone())
        };
        session.update(cx, |session, cx| session.want_stored_queries(cx));
        Some(ContextMenu::build_persistent(
            window,
            cx,
            move |mut menu, _, cx| {
                let marks = cx
                    .try_global::<StudioSettings>()
                    .map(|settings| settings.effect_marks(&graph))
                    .unwrap_or_default();
                let catalog = session.read(cx).stored_queries.loaded().cloned();
                let views = cx
                    .try_global::<StudioSettings>()
                    .map(|settings| settings.graph_views(&graph).to_vec())
                    .unwrap_or_default();
                let Some(catalog) = catalog else {
                    return menu.header("Reading the stored queries…");
                };
                let mut choices: Vec<(String, String)> = catalog
                    .iter()
                    .filter(|query| !query.mutation && query.params.iter().all(|p| p.nullable))
                    .map(|query| (query.name.clone(), query.name.clone()))
                    .collect();
                choices.extend(
                    views
                        .iter()
                        .filter(|view| view.query.as_ref().is_some_and(|q| !q.stored.is_empty()))
                        .map(|view| {
                            (
                                studio_settings::effect_view_key(&view.key()),
                                format!("{} (view)", view.name),
                            )
                        }),
                );
                if choices.is_empty() {
                    return menu.header(
                        "No stored read without parameters, nor a view of one, to run here",
                    );
                }
                menu = menu.header("Reads a review of this graph runs");
                for (key, label) in choices {
                    let mark = marks.get(&key).copied();
                    let (icon, color) = match mark {
                        Some(EffectMark::Check) => (IconName::Check, Color::Success),
                        Some(EffectMark::Watched) => (IconName::Eye, Color::Accent),
                        None => (IconName::Dash, Color::Disabled),
                    };
                    let graph = graph.clone();
                    menu = menu.submenu_with_colored_icon(label, icon, color, move |menu, _, _| {
                        let choice =
                            |menu: ContextMenu, text: &'static str, set: Option<EffectMark>| {
                                let (graph, key) = (graph.clone(), key.clone());
                                menu.toggleable_entry(
                                    text,
                                    mark == set,
                                    IconPosition::Start,
                                    None,
                                    move |_, cx| {
                                        studio_settings::update(cx, |settings| {
                                            settings.set_effect_mark(&graph, &key, set)
                                        })
                                    },
                                )
                            };
                        let menu = choice(
                            menu,
                            "Watch: show the rows it gains and loses",
                            Some(EffectMark::Watched),
                        );
                        let menu = choice(
                            menu,
                            "Check: it should return nothing",
                            Some(EffectMark::Check),
                        );
                        choice(menu, "Leave out", None)
                    });
                }
                menu
            },
        ))
    }

    fn render_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let effect = self.effect.read(cx);
        let view = cx.entity().downgrade();
        let pick = PopoverMenu::new("effect-pick")
            .trigger(
                studio_ui::ds::chip_button("effect-pick-button", IconName::ListTodo, "Reads", None)
                    .end_icon(Icon::new(IconName::ChevronDown)),
            )
            .with_handle(self.picker_menu.clone())
            .menu(move |window, cx| Self::picker(view.clone(), window, cx));
        let focus = self.focus_handle.clone();
        let refresh = studio_ui::ds::icon_button("effect-refresh", IconName::RotateCw)
            .tip_fn(move |_, cx| {
                studio_ui::ds::tooltip_for_action_in_view(
                    "Read every read again at both heads",
                    &Refresh,
                    &focus,
                    cx,
                )
            })
            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx)));
        let mut facts: Vec<AnyElement> = Vec::new();
        match effect.heads() {
            Loadable::Loaded(heads) => {
                facts.push(
                    studio_ui::ds::meta_code(
                        IconName::GitBranch,
                        format!("{} at {}", self.target, short_commit_id(&heads.target)),
                        cx,
                    )
                    .into_any_element(),
                );
                facts.push(
                    studio_ui::ds::meta_code(
                        IconName::GitBranch,
                        format!("{} at {}", self.branch, short_commit_id(&heads.branch)),
                        cx,
                    )
                    .into_any_element(),
                );
            }
            Loadable::Failed(error) => facts.push(
                studio_ui::ds::text::error(format!("The heads could not be read: {error}"))
                    .into_any_element(),
            ),
            _ if effect.reads().is_empty() => {}
            _ => facts.push(studio_ui::ds::text::meta("Reading both heads…").into_any_element()),
        }
        let mut notices = Vec::new();
        if effect.moved {
            notices.push(
                studio_ui::ds::callout(
                    Severity::Info,
                    format!(
                        "Newer data: {} or {} moved since these reads.",
                        self.branch, self.target
                    ),
                )
                .actions_slot(
                    studio_ui::ds::state_action(
                        "effect-newer-refresh",
                        "Refresh",
                        &Refresh,
                        &self.focus_handle,
                        cx,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                )
                .into_any_element(),
            );
        }
        if let Some(heads) = effect.heads().loaded().filter(|heads| heads.target_moved()) {
            notices.push(target_moved_notice(heads, &self.target));
        }
        v_flex()
            .flex_none()
            .border_b_1()
            .border_color(studio_ui::ds::surface::rule(cx))
            .child(
                v_flex()
                    .px(px(space::EDGE))
                    .py(px(space::LG))
                    .gap(px(space::SM))
                    .child(
                        h_flex()
                            .gap(px(space::INLINE))
                            .items_center()
                            .children(studio_workspace::view_strip(Item::views(self, cx), cx))
                            .child(div().flex_1())
                            .child(
                                h_flex()
                                    .gap(px(space::CONTROLS))
                                    .items_center()
                                    .child(studio_ui::ds::control_chip("effect-pick-chip", pick))
                                    .child(refresh),
                            ),
                    )
                    .child(studio_ui::ds::text::heading(format!(
                        "{} → {}",
                        self.branch, self.target
                    )))
                    .when(!facts.is_empty(), |header| {
                        header.child(studio_ui::ds::meta_row(facts).flex_wrap())
                    }),
            )
            .children(notices)
            .into_any_element()
    }

    /// The reads, checks first, as the cached list draws them.
    pub(crate) fn render_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let effect = self.effect.read(cx);
        let held = effect.merge_result().held().is_some()
            || matches!(effect.merge_result(), MergeResult::Publishing { .. });
        let has_marks = effect.has_marks();
        // Each read's sides are shared, so this copies a few names and counts.
        let all: Vec<EffectRead> = effect.reads().to_vec();
        if all.is_empty() {
            return studio_ui::ds::empty_state(
                IconName::ListTodo,
                "No read is marked for this graph's reviews",
            )
            .child(studio_ui::ds::state_detail(
                "Pick the stored reads a review runs at both heads: a queue or a roll-up to \
                 watch, or a check that should return nothing. The graph cannot declare them \
                 yet (E64), so the marks are yours, kept with this graph's settings.",
            ))
            .child(
                studio_ui::ds::state_action(
                    "effect-pick-empty",
                    "Pick reads",
                    &PickReads,
                    &self.focus_handle,
                    cx,
                )
                .on_click(|_, window, cx| window.dispatch_action(Box::new(PickReads), cx)),
            )
            .into_any_element();
        }
        let mut list = v_flex().w_full().pb(px(space::BETWEEN));
        let sections: [(&str, bool); 2] = [("Checks", true), ("Watched", false)];
        for (title, checks) in sections {
            let reads: Vec<&EffectRead> = all
                .iter()
                .filter(|read| read.is_check() == checks)
                .collect();
            if reads.is_empty() {
                if !checks && !has_marks {
                    list = list.child(
                        v_flex()
                            .px(px(space::EDGE))
                            .pt(px(space::BETWEEN - space::XS))
                            .gap(px(space::SM))
                            .child(studio_ui::ds::text::meta(title.to_owned()))
                            .child(studio_ui::ds::text::muted(
                                "No read is watched. Reads lists the stored reads to watch.",
                            )),
                    );
                }
                continue;
            }
            list = list.child(self.render_columns(title, held, cx));
            for read in reads {
                list = list.child(self.render_read(read, held, cx));
                if self.open.contains(&read.key) {
                    list = list.child(render_rows_of(read, &self.target, held, cx));
                }
            }
        }
        div()
            .id("effect-list")
            .size_full()
            .overflow_y_scroll()
            .child(list)
            .into_any_element()
    }

    /// A section's heading, with the columns' names at their places.
    fn render_columns(&self, title: &str, held: bool, cx: &App) -> AnyElement {
        h_flex()
            .px(px(space::EDGE))
            .pt(px(space::BETWEEN - space::XS))
            .pb(px(space::XS))
            .gap(px(space::INLINE))
            .items_center()
            .child(studio_ui::ds::text::meta(title.to_owned()))
            .child(div().flex_1())
            .child(count_cell(studio_ui::ds::text::meta(self.target.clone())))
            .child(count_cell(studio_ui::ds::text::meta(self.branch.clone())))
            .when(held, |row| {
                row.child(count_cell(studio_ui::ds::text::meta("Merge result")))
            })
            .child(count_cell(studio_ui::ds::text::meta("Moved")))
            .border_b_1()
            .border_color(studio_ui::ds::surface::rule(cx))
            .into_any_element()
    }

    fn render_read(&self, read: &EffectRead, held: bool, cx: &mut Context<Self>) -> AnyElement {
        let selected = self.selected.as_deref() == Some(read.key.as_str());
        let open = self.open.contains(&read.key);
        let (icon, color) = if read.unrunnable.is_some() {
            (IconName::Dash, Color::Disabled)
        } else if read.fails() {
            (IconName::Warning, Color::Warning)
        } else if read.is_check() {
            match &read.at_branch {
                Some(Side::Rows(_)) => (IconName::Check, Color::Success),
                Some(Side::Failed(_)) => (IconName::XCircle, Color::Error),
                _ => (IconName::Circle, Color::Muted),
            }
        } else {
            (IconName::Eye, Color::Muted)
        };
        let kind = if read.is_builtin() {
            Some("Studio's check")
        } else if matches!(
            &read.source,
            Source::Stored {
                params: Some(_),
                ..
            }
        ) {
            Some("view")
        } else {
            None
        };
        let key = read.key.clone();
        let tip = read
            .unrunnable
            .clone()
            .or_else(|| read.description.clone())
            .unwrap_or_else(|| read.label.clone());
        let target_cell = if read.is_builtin() {
            count_cell(studio_ui::ds::text::meta("—"))
        } else {
            side_cell(read.at_target.as_ref(), false)
        };
        let branch_cell = side_cell(read.at_branch.as_ref(), read.is_check());
        let moved = match (&read.diff, read.is_builtin()) {
            (_, true) => String::new(),
            (Some(diff), _) if diff.is_empty() => "same".to_owned(),
            (Some(diff), _) => diff.counts(),
            (None, _) => String::new(),
        };
        h_flex()
            .id(SharedString::from(format!("effect-read-{}", read.key)))
            .h(px(size::FEED_ROW))
            .px(px(space::EDGE))
            .gap(px(space::INLINE))
            .items_center()
            .when(selected, |row| {
                row.bg(studio_ui::ds::surface::selected_document(cx))
            })
            .hover(|row| row.bg(studio_ui::ds::surface::hover(cx)))
            .tip(studio_ui::ds::tooltip(tip))
            .child(studio_ui::ds::icon::meta(if open {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            }))
            .child(studio_ui::ds::icon::meta(icon).color(color))
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(space::INLINE))
                    .child(studio_ui::ds::text::plain(read.label.clone()).truncate())
                    .children(kind.map(|kind| studio_ui::ds::tag(kind, cx))),
            )
            .child(target_cell)
            .child(branch_cell)
            .when(held, |row| {
                row.child(if read.is_builtin() {
                    count_cell(studio_ui::ds::text::meta("—"))
                } else {
                    side_cell(read.at_result.as_ref(), read.is_check())
                })
            })
            .child(count_cell(studio_ui::ds::text::meta(moved)))
            .on_click(cx.listener(move |this, _, window, cx| {
                // The keys go on from the read clicked.
                window.focus(&this.focus_handle, cx);
                this.toggle(key.clone(), cx)
            }))
            .into_any_element()
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> AnyElement {
        let effect = self.effect.read(cx);
        let failing_on_result = effect.checks().filter(|r| r.fails_on_result()).count();
        let target = self.target.clone();
        let (status, icon): (String, Option<AnyElement>) = match effect.merge_result() {
            MergeResult::None => (
                format!(
                    "Check the merge result: merge as an unpublished commit, run these reads \
                     on it, then publish it onto {target} or drop it."
                ),
                None,
            ),
            MergeResult::Merging => (
                "Merging as an unpublished commit…".to_owned(),
                Some(studio_ui::ds::spinner().into_any_element()),
            ),
            MergeResult::Held { commit } => (
                if failing_on_result > 0 {
                    format!(
                        "Merge result {} is held, unpublished: {} on it. {target} has not moved.",
                        short_commit_id(commit),
                        studio_ui::plural(failing_on_result, "check has rows", "checks have rows"),
                    )
                } else {
                    format!(
                        "Merge result {} is held, unpublished. {target} has not moved.",
                        short_commit_id(commit)
                    )
                },
                Some(
                    studio_ui::ds::icon::meta(if failing_on_result > 0 {
                        IconName::Warning
                    } else {
                        IconName::GitCommit
                    })
                    .color(if failing_on_result > 0 {
                        Color::Warning
                    } else {
                        Color::Muted
                    })
                    .into_any_element(),
                ),
            ),
            MergeResult::Publishing { .. } => (
                format!("Publishing onto {target}…"),
                Some(studio_ui::ds::spinner().into_any_element()),
            ),
            MergeResult::Dropping { .. } => (
                "Dropping the merge result…".to_owned(),
                Some(studio_ui::ds::spinner().into_any_element()),
            ),
            MergeResult::Published { commit } => (
                format!("Published {} onto {target}.", short_commit_id(commit)),
                Some(
                    studio_ui::ds::icon::meta(IconName::Check)
                        .color(Color::Success)
                        .into_any_element(),
                ),
            ),
            MergeResult::Dropped => (
                format!("Dropped the merge result; {target} is as it was."),
                None,
            ),
            MergeResult::AlreadyMerged => (
                format!(
                    "{} is already in {target}: there is no merge to check.",
                    self.branch
                ),
                None,
            ),
            MergeResult::Refused {
                step,
                reason,
                commit,
            } => (
                match (step, commit) {
                    (ResultStep::Publish, Some(_)) => format!(
                        "The server refused to publish the merge result: {reason}. It is still \
                         held; drop it, then check again."
                    ),
                    _ => format!(
                        "The server refused to {} it: {reason}. Nothing changed.",
                        step.verb()
                    ),
                },
                Some(
                    studio_ui::ds::icon::meta(IconName::XCircle)
                        .color(Color::Error)
                        .into_any_element(),
                ),
            ),
            MergeResult::Unknown { step, reason, .. } => (
                format!(
                    "The answer to the {} was lost ({reason}). Studio did not send it again; \
                     open History to see whether it landed.",
                    step.verb()
                ),
                Some(
                    studio_ui::ds::icon::meta(IconName::Warning)
                        .color(Color::Warning)
                        .into_any_element(),
                ),
            ),
        };
        let focus = self.focus_handle.clone();
        let mut buttons: Vec<AnyElement> = Vec::new();
        if let Some(commit) = effect.merge_result().held().map(str::to_owned) {
            let drop = studio_ui::ds::button("effect-drop", "Drop")
                .style(ButtonStyle::Outlined)
                .key_binding(KeyBinding::for_action_in(&DropMergeResult, &focus, cx))
                .tip_fn({
                    let title = format!(
                        "Drop the unpublished commit; {} stays as it is",
                        self.target
                    );
                    let cli = format!("omnigraph commit drop {commit}");
                    move |_, cx| {
                        studio_ui::ds::tooltip_with_meta(title.clone(), None, cli.clone(), cx)
                    }
                })
                .on_click(cx.listener(|this, _, _, cx| {
                    this.effect.update(cx, |effect, cx| effect.drop_result(cx))
                }));
            let refused = effect.publish_refusal(cx);
            let publish =
                studio_ui::ds::button("effect-publish", format!("Publish onto {}", self.target))
                    .style(ButtonStyle::Filled)
                    .when(refused.is_none(), |button| {
                        button.key_binding(KeyBinding::for_action_in(
                            &PublishMergeResult,
                            &focus,
                            cx,
                        ))
                    })
                    .disabled(refused.is_some())
                    .tip_fn({
                        let title = refused.clone().unwrap_or_else(|| {
                            format!("Publish the merge result: {} moves to it", self.target)
                        });
                        let cli = format!("omnigraph commit publish {commit}");
                        move |_, cx| {
                            studio_ui::ds::tooltip_with_meta(title.clone(), None, cli.clone(), cx)
                        }
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.effect.update(cx, |effect, cx| effect.publish(cx))
                    }));
            buttons.push(drop.into_any_element());
            buttons.push(publish.into_any_element());
        } else {
            let refused = effect.result_refusal(cx);
            let check = studio_ui::ds::button("effect-check-result", "Check the merge result")
                .style(ButtonStyle::Outlined)
                .when(refused.is_none(), |button| {
                    button.key_binding(KeyBinding::for_action_in(&CheckMergeResult, &focus, cx))
                })
                .disabled(refused.is_some())
                .tip_fn({
                    let title = refused.clone().unwrap_or_else(|| {
                        format!(
                            "Merge {} into {} as an unpublished commit and run the reads on it",
                            self.branch, self.target
                        )
                    });
                    let cli = format!(
                        "omnigraph branch merge {} --into {} --as-commit",
                        self.branch, self.target
                    );
                    move |_, cx| {
                        studio_ui::ds::tooltip_with_meta(title.clone(), None, cli.clone(), cx)
                    }
                })
                .on_click(cx.listener(|this, _, _, cx| {
                    this.effect
                        .update(cx, |effect, cx| effect.check_merge_result(cx))
                }));
            buttons.push(check.into_any_element());
        }
        h_flex()
            .flex_none()
            .px(px(space::EDGE))
            .py(px(space::LG))
            .gap(px(space::CONTROLS))
            .items_center()
            .border_t_1()
            .border_color(studio_ui::ds::surface::rule(cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(space::INLINE))
                    .items_center()
                    .children(icon)
                    .child(div().min_w_0().child(studio_ui::ds::text::body(status))),
            )
            .children(buttons)
            .into_any_element()
    }
}

impl Render for EffectView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let header = self.render_header(cx);
        let list = AnyView::from(self.list.clone()).cached(StyleRefinement::default().size_full());
        let effect = self.effect.read(cx);
        let held = effect.merge_result().held().is_some();
        let can_check = effect.result_refusal(cx).is_none();
        let can_publish = held && effect.publish_refusal(cx).is_none();
        v_flex()
            .key_context("Effect")
            .track_focus(&self.focus_handle)
            .size_full()
            .on_action(cx.listener(|this, _: &NextRead, _, cx| this.select_step(1, cx)))
            .on_action(cx.listener(|this, _: &PreviousRead, _, cx| this.select_step(-1, cx)))
            .on_action(cx.listener(|this, _: &ToggleRead, _, cx| {
                if let Some(key) = this.selected.clone() {
                    this.toggle(key, cx);
                }
            }))
            .on_action(
                cx.listener(|this, _: &PickReads, window, cx| this.picker_menu.toggle(window, cx)),
            )
            // The writes are registered only while they may run (constitution VIII).
            .when(can_check && !held, |view| {
                view.on_action(cx.listener(|this, _: &CheckMergeResult, _, cx| {
                    this.effect
                        .update(cx, |effect, cx| effect.check_merge_result(cx))
                }))
            })
            .when(can_publish, |view| {
                view.on_action(cx.listener(|this, _: &PublishMergeResult, _, cx| {
                    this.effect.update(cx, |effect, cx| effect.publish(cx))
                }))
            })
            .when(held, |view| {
                view.on_action(cx.listener(|this, _: &DropMergeResult, _, cx| {
                    this.effect.update(cx, |effect, cx| effect.drop_result(cx))
                }))
            })
            .on_action(cx.listener(|this, _: &Refresh, _, cx| {
                this.refresh(cx);
                cx.propagate();
            }))
            .child(header)
            .child(div().flex_1().min_h_0().child(list))
            .child(self.render_footer(cx))
    }
}

/// A notice that the target moved since the fork, so a row it gained reads as removed.
fn target_moved_notice(heads: &Heads, target: &str) -> AnyElement {
    studio_ui::ds::callout(
        Severity::Info,
        format!(
            "{target} moved since the branch forked from it at {}: a row {target} gained since \
             reads as removed here. Check the merge result to see what a merge would leave.",
            short_commit_id(&heads.base)
        ),
    )
    .into_any_element()
}

/// A column's cell of a count, at its fixed width.
fn count_cell(content: impl IntoElement) -> Div {
    h_flex()
        .w(px(size::FIELD_LABEL))
        .flex_none()
        .justify_end()
        .child(content)
}

/// One side of a read in its column: a spinner, its row count, or why it has none. A check's
/// rows read as a warning.
fn side_cell(side: Option<&Side>, check: bool) -> Div {
    match side {
        None => count_cell(studio_ui::ds::text::meta("—")),
        Some(Side::Reading) => count_cell(studio_ui::ds::spinner()),
        Some(Side::Failed(error)) => count_cell(
            div()
                .id(SharedString::from(format!("effect-failed-{}", error.len())))
                .tip(studio_ui::ds::tooltip(error.clone()))
                .child(studio_ui::ds::text::body("Failed").color(Color::Error)),
        ),
        Some(Side::Rows(rows)) => {
            let text = studio_ui::plural(rows.row_count, "row", "rows");
            count_cell(if check && rows.row_count > 0 {
                studio_ui::ds::text::body(text).color(Color::Warning)
            } else {
                studio_ui::ds::text::body(text)
            })
        }
    }
}

/// An unfolded read: the rows the branch adds, takes away and changes, against the target's,
/// and against the merge result's while one is held; Studio's check lists its rows.
fn render_rows_of(read: &EffectRead, target: &str, held: bool, cx: &App) -> AnyElement {
    let inset = v_flex()
        .pl(px(space::EDGE + space::XXL))
        .pr(px(space::EDGE))
        .py(px(space::SM))
        .gap(px(space::WITHIN))
        .border_b_1()
        .border_color(studio_ui::ds::surface::rule(cx));
    if let Some(why) = &read.unrunnable {
        return inset
            .child(studio_ui::ds::text::muted(why.clone()))
            .into_any_element();
    }
    let failed = [&read.at_target, &read.at_branch, &read.at_result]
        .into_iter()
        .flatten()
        .find_map(|side| match side {
            Side::Failed(error) => Some(error.clone()),
            _ => None,
        });
    if let Some(error) = failed {
        return inset
            .child(studio_ui::ds::text::error(error))
            .into_any_element();
    }
    if read.is_builtin() {
        return match read.at_branch.as_ref().and_then(Side::rows) {
            Some(rows) if rows.row_count == 0 => inset
                .child(studio_ui::ds::text::muted(format!(
                    "No relationship this branch adds is already on {target}."
                )))
                .children(rows.partial.clone().map(studio_ui::ds::text::meta))
                .into_any_element(),
            Some(rows) => inset
                .children(rows.cells.iter().take(ROWS_SHOWN).map(|cells| {
                    let text = |i: usize| cells.get(i).map(cell_text).unwrap_or_default();
                    row_line(
                        "!",
                        Color::Warning,
                        format!("{} {} → {}: {}", text(0), text(1), text(2), text(3)),
                    )
                }))
                .children(more_line(rows.row_count))
                .children(rows.partial.clone().map(studio_ui::ds::text::meta))
                .into_any_element(),
            None => inset
                .child(studio_ui::ds::text::muted("Reading…"))
                .into_any_element(),
        };
    }
    let mut inset = inset;
    let pairs = [
        (read.at_branch.as_ref(), read.diff.as_ref(), "this branch"),
        (
            read.at_result.as_ref().filter(|_| held),
            read.result_diff.as_ref().filter(|_| held),
            "the merge result",
        ),
    ];
    for (side, diff, against) in pairs {
        let (Some(after), Some(before), Some(diff)) = (
            side.and_then(Side::rows),
            read.at_target.as_ref().and_then(Side::rows),
            diff,
        ) else {
            continue;
        };
        if held {
            inset = inset.child(studio_ui::ds::text::meta(format!("{target} → {against}")));
        }
        inset = inset.children(diff_lines(before, after, diff, target));
    }
    if read.diff.is_none() {
        inset = inset.child(studio_ui::ds::text::muted("Reading…"));
    }
    inset.into_any_element()
}

/// The lines of a row diff: rows added, removed, then changed, each kind at most
/// [`ROWS_SHOWN`].
fn diff_lines(before: &Rows, after: &Rows, diff: &RowDiff, target: &str) -> Vec<AnyElement> {
    if diff.is_empty() {
        return vec![
            studio_ui::ds::text::muted(format!("The same rows as on {target}.")).into_any_element(),
        ];
    }
    let mut lines: Vec<AnyElement> = Vec::new();
    lines.extend(
        diff.added
            .iter()
            .take(ROWS_SHOWN)
            .map(|&at| row_line("+", Color::Created, row_summary(&after.cells[at]))),
    );
    lines.extend(more_line(diff.added.len()));
    lines.extend(
        diff.removed
            .iter()
            .take(ROWS_SHOWN)
            .map(|&at| row_line("−", Color::Deleted, row_summary(&before.cells[at]))),
    );
    lines.extend(more_line(diff.removed.len()));
    lines.extend(diff.changed.iter().take(ROWS_SHOWN).map(|change| {
        let key = match diff.matched_by {
            graph_store::row_diff::MatchedBy::Column(column) => after.cells[change.after]
                .get(column)
                .map(cell_text)
                .unwrap_or_default(),
            graph_store::row_diff::MatchedBy::WholeRow => String::new(),
        };
        let moved: Vec<String> = change
            .columns
            .iter()
            .map(|&column| {
                format!(
                    "{} {} → {}",
                    after.columns.get(column).map(String::as_str).unwrap_or(""),
                    before.cells[change.before]
                        .get(column)
                        .map(cell_text)
                        .unwrap_or_default(),
                    after.cells[change.after]
                        .get(column)
                        .map(cell_text)
                        .unwrap_or_default(),
                )
            })
            .collect();
        row_line(
            "~",
            Color::Modified,
            format!("{key}  {}", moved.join(" · ")),
        )
    }));
    lines.extend(more_line(diff.changed.len()));
    if diff.capped {
        lines.push(
            studio_ui::ds::text::meta(format!(
                "Compared on the first {} rows of each side.",
                studio_ui::group_digits(graph_store::row_diff::COMPARED_ROWS)
            ))
            .into_any_element(),
        );
    }
    lines
}

/// "And 120 more" under a kind of row past [`ROWS_SHOWN`].
fn more_line(count: usize) -> Option<AnyElement> {
    (count > ROWS_SHOWN).then(|| {
        studio_ui::ds::text::meta(format!(
            "and {} more",
            studio_ui::group_digits(count - ROWS_SHOWN)
        ))
        .into_any_element()
    })
}

/// One listed row: its kind's mark, then its first cells.
fn row_line(mark: &'static str, color: Color, text: String) -> AnyElement {
    h_flex()
        .gap(px(space::INLINE))
        .min_w_0()
        .child(studio_ui::ds::text::body(mark).color(color))
        .child(studio_ui::ds::text::body(text).truncate())
        .into_any_element()
}

/// A row's first cells, as words.
fn row_summary(cells: &[Value]) -> String {
    cells
        .iter()
        .filter(|cell| !cell.is_null())
        .take(CELLS_SHOWN)
        .map(cell_text)
        .collect::<Vec<_>>()
        .join("  ")
}

/// A cell's value in a few words: a text as it is, a node by its `@id`, a list joined.
fn cell_text(value: &Value) -> String {
    match value {
        Value::Null => "—".to_owned(),
        Value::String(text) => text.clone(),
        Value::Object(node) => node
            .get("@id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| value.to_string()),
        Value::Array(items) => items.iter().map(cell_text).collect::<Vec<_>>().join(", "),
        // A sum of decimals carries binary noise (142.6 − 3.1 = 139.49999999999997): shown to
        // nine places, as a person wrote it.
        Value::Number(number) if number.is_f64() => number
            .as_f64()
            .map(|x| ((x * 1e9).round() / 1e9).to_string())
            .unwrap_or_else(|| number.to_string()),
        other => other.to_string(),
    }
}

/// The Effect's list of reads as a view of its own, drawn cached and again when the tab or its
/// Effect notifies (`docs/drawing.md`): a menu opening over the tab, a tooltip or a hover on
/// the footer leaves it as it was.
pub(crate) struct EffectRows {
    view: WeakEntity<EffectView>,
    _changed: Vec<Subscription>,
    #[cfg(test)]
    pub(crate) renders: usize,
}

impl EffectRows {
    /// Made with the tab, not on its first draw.
    fn new(cx: &mut Context<EffectView>) -> Entity<Self> {
        let view = cx.entity();
        cx.new(|cx| EffectRows {
            view: view.downgrade(),
            _changed: vec![cx.observe(&view, |_, _, cx| cx.notify())],
            #[cfg(test)]
            renders: 0,
        })
    }
}

impl Render for EffectRows {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(test)]
        {
            self.renders += 1;
        }
        let list = self
            .view
            .update(cx, |view, cx| view.render_list(cx))
            .unwrap_or_else(|_| gpui::Empty.into_any_element());
        v_flex().size_full().child(list)
    }
}

/// Opens the Effect of `branch` against `target`.
pub fn open_effect(
    workspace: &mut Workspace,
    branch: String,
    target: String,
    preview: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(session) = workspace.session().cloned() else {
        return;
    };
    if branch == target {
        workspace.show_toast(
            studio_workspace::ToastKind::Info,
            format!(
                "You are on {target}. Switch to another branch to see what it does to {target}."
            ),
            cx,
        );
        return;
    }
    let key = effect_key(&branch, &target);
    workspace.open_item_with::<EffectView>(key, preview, window, cx, |window, cx| {
        EffectView::new(session, branch, target, window, cx)
    });
}

#[cfg(test)]
mod tests {
    use graph_store::effect::{DUPLICATE_EDGES, Hold};
    use serde_json::json;

    use super::*;
    use crate::view::tests::fake_server;

    const SCHEMA: &str = "node Issue {\n  slug: String @key\n  title: String?\n}\n\
                          node Person {\n  slug: String @key\n}\n\
                          edge OwnedBy: Issue -> Person\n";

    /// A server whose `ready` read gains `c` and loses `a` on the branch, and whose branch adds
    /// an owner edge main already has, twice; a merge `as_commit` is held as `u1`.
    fn effect_server(
        cx: &mut gpui::TestAppContext,
    ) -> (
        Entity<GraphSession>,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        effect_server_with(Scenario::default(), cx)
    }

    /// What [`effect_server`] does otherwise.
    #[derive(Clone, Copy, Default)]
    struct Scenario {
        /// The branch removes main's owner edge and adds it once again: a replacement.
        replaces: bool,
        /// The publish's answer is lost; the commit reads as published.
        loses_publish: bool,
    }

    fn effect_server_with(
        scenario: Scenario,
        cx: &mut gpui::TestAppContext,
    ) -> (
        Entity<GraphSession>,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        let commit = |id: &str, kind: &str| json!({ "graph_commit_id": id, "graph_manifest_version": 1, "kind": kind, "created_at": 0, "parent_commit_id": "m2", "merged_parent_commit_id": "f2" });
        fake_server(
            move |path: &str, query: &str, sent: &str| {
                let rows = |slugs: &[&str]| {
                    let rows: Vec<Value> = slugs
                        .iter()
                        .map(|slug| json!({ "i.slug": slug, "i.title": slug.to_uppercase() }))
                        .collect();
                    json!({ "query_name": "ready", "target": { "snapshot": "x" }, "row_count": rows.len(), "columns": ["i.slug", "i.title"], "rows": rows })
                };
                let edge = json!({ "kind": "edge", "type": { "id": "OwnedBy", "name": "OwnedBy" }, "id": "e1", "op": "insert", "after": { "properties": {}, "endpoints": { "from": "i1", "to": "p1" } } });
                let mut twice = edge.clone();
                twice["id"] = json!("e2");
                if path.ends_with("/schema") {
                    (200, json!({ "schema_source": SCHEMA }))
                } else if path.ends_with("/branches/merge-base") {
                    (
                        200,
                        json!({ "merge_base": commit("m1", "published"), "source": "feature", "source_commit_id": "f2", "target": "main", "target_commit_id": "m2" }),
                    )
                } else if path.ends_with("/queries") {
                    (
                        200,
                        json!({ "queries": [{ "name": "ready", "tool_name": "ready", "mutation": false, "params": [] }] }),
                    )
                } else if path.ends_with("/queries/ready") {
                    let slugs: &[&str] = if sent.contains("\"m2\"") {
                        &["a", "b"]
                    } else {
                        &["b", "c"]
                    };
                    (200, rows(slugs))
                } else if path.ends_with("/diff") && query.contains("op=delete") {
                    let removed: Vec<Value> = if scenario.replaces {
                        vec![
                            json!({ "kind": "edge", "type": { "id": "OwnedBy", "name": "OwnedBy" }, "id": "e0", "op": "delete", "before": { "properties": {}, "endpoints": { "from": "i1", "to": "p1" } } }),
                        ]
                    } else {
                        Vec::new()
                    };
                    (
                        200,
                        json!({ "from_commit_id": "m1", "to_commit_id": "f2", "changes": removed }),
                    )
                } else if path.ends_with("/diff") && query.contains("kind=edge") {
                    let added = if scenario.replaces {
                        vec![edge.clone()]
                    } else {
                        vec![edge.clone(), twice.clone()]
                    };
                    (
                        200,
                        json!({ "from_commit_id": "m1", "to_commit_id": "f2", "changes": added }),
                    )
                } else if path.ends_with("/query") && sent.contains("studio_edges_between") {
                    (
                        200,
                        json!({ "query_name": "studio_edges_between", "target": { "snapshot": "m2" }, "row_count": 1, "columns": ["edge_id", "from_id", "to_id"], "rows": [{ "edge_id": "e0", "from_id": "i1", "to_id": "p1" }] }),
                    )
                } else if path.ends_with("/branches/merge") && sent.contains("\"as_commit\":true") {
                    (
                        200,
                        json!({ "source": "feature", "target": "main", "outcome": "fast_forward", "commit": commit("u1", "unpublished") }),
                    )
                } else if path.ends_with("/commits/u1/publish") && scenario.loses_publish {
                    // A gateway's error, without the server's contract header: the outcome is
                    // not known.
                    (502, json!({ "error": "bad gateway" }))
                } else if path.ends_with("/commits/u1/publish") || path.ends_with("/commits/u1") {
                    // The publish's answer, and the commit read once it is published.
                    (200, commit("u1", "published"))
                } else {
                    (200, json!({ "branches": ["main", "feature"] }))
                }
            },
            cx,
        )
    }

    #[test]
    fn a_cells_value_reads_as_written() {
        assert_eq!(cell_text(&json!(139.49999999999997)), "139.5");
        assert_eq!(cell_text(&json!(27.7)), "27.7");
        assert_eq!(cell_text(&json!(3)), "3");
        assert_eq!(cell_text(&json!({"@id": "lk-acl"})), "lk-acl");
        assert_eq!(cell_text(&json!(["a", "b"])), "a, b");
    }

    fn init(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            studio_settings::init_test(cx);
            studio_settings::keep_changes_in_memory(cx);
            editor::init(cx);
            // Nothing turns while draws are counted.
            cx.set_reduce_motion(true);
        });
    }

    /// The Effect runs a watched read at both heads and says which rows came and went; Studio's
    /// own check finds the relationship the branch adds that main already has, and holds a
    /// merge; the merge result is merged as an unpublished commit, the reads run on it, and it
    /// is published once, onto main.
    #[gpui::test]
    async fn the_effect_compares_both_heads_and_checks_the_merge_result(
        cx: &mut gpui::TestAppContext,
    ) {
        init(cx);
        let (session, asked) = effect_server(cx);
        let graph = session.read_with(cx, |session, _| session.overrides_key());
        cx.update(|cx| {
            studio_settings::update(cx, |settings| {
                settings.set_effect_mark(&graph, "ready", Some(EffectMark::Watched))
            })
        });
        let (view, cx) = cx.add_window_view(|window, cx| {
            EffectView::new(session, "feature".into(), "main".into(), window, cx)
        });
        cx.run_until_parked();
        let effect = view.read_with(cx, |view, _| view.effect.clone());
        effect.read_with(cx, |effect, _| {
            let keys: Vec<&str> = effect.reads().iter().map(|r| r.key.as_str()).collect();
            assert_eq!(keys, [DUPLICATE_EDGES, "ready"], "checks first");
            let builtin = &effect.reads()[0];
            let rows = builtin.at_branch.as_ref().and_then(Side::rows).unwrap();
            assert_eq!(
                rows.cells,
                vec![vec![
                    json!("OwnedBy"),
                    json!("i1"),
                    json!("p1"),
                    json!("on main, and added 2 times here")
                ]]
            );
            let ready = &effect.reads()[1];
            let diff = ready.diff.as_ref().expect("both sides compared");
            assert_eq!(
                (diff.added.clone(), diff.removed.clone()),
                (vec![1], vec![0])
            );
            assert_eq!(diff.counts(), "+1 −1");
            assert_eq!(
                effect.hold(Some("f2")),
                Hold::Rows(vec!["Relationship already on main".into()])
            );
            assert_eq!(effect.hold(Some("f1")), Hold::Unread, "another head");
        });

        effect.update(cx, |effect, cx| effect.check_merge_result(cx));
        cx.run_until_parked();
        effect.read_with(cx, |effect, _| {
            assert_eq!(
                effect.merge_result(),
                &MergeResult::Held {
                    commit: "u1".into()
                }
            );
            let ready = &effect.reads()[1];
            assert!(ready.at_result.as_ref().and_then(Side::rows).is_some());
            assert_eq!(ready.result_diff.as_ref().unwrap().counts(), "+1 −1");
        });
        let merge_sent = asked
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.contains("\"as_commit\":true") && !line.contains("delete_branch"));
        assert!(merge_sent, "merged as a commit, the branch kept");
        effect.update(cx, |effect, cx| effect.publish(cx));
        cx.run_until_parked();
        effect.read_with(cx, |effect, _| {
            assert_eq!(
                effect.merge_result(),
                &MergeResult::Published {
                    commit: "u1".into()
                }
            )
        });
        let published = asked
            .lock()
            .unwrap()
            .iter()
            .filter(|line| line.contains("/commits/u1/publish"))
            .count();
        assert_eq!(published, 1, "published once");
    }

    /// A relationship the branch removes and adds again between the same ends replaces main's:
    /// a merge leaves one, so Studio's check finds nothing.
    #[gpui::test]
    async fn a_replaced_relationship_is_no_duplicate(cx: &mut gpui::TestAppContext) {
        init(cx);
        let scenario = Scenario {
            replaces: true,
            ..Scenario::default()
        };
        let (session, _asked) = effect_server_with(scenario, cx);
        let (view, cx) = cx.add_window_view(|window, cx| {
            EffectView::new(session, "feature".into(), "main".into(), window, cx)
        });
        cx.run_until_parked();
        let effect = view.read_with(cx, |view, _| view.effect.clone());
        effect.read_with(cx, |effect, _| {
            let builtin = &effect.reads()[0];
            let rows = builtin.at_branch.as_ref().and_then(Side::rows).unwrap();
            assert_eq!(rows.row_count, 0);
            assert_eq!(effect.hold(Some("f2")), Hold::Clear);
        });
    }

    /// A publish whose answer was lost is not sent again: Refresh reads the commit, which says
    /// it was published.
    #[gpui::test]
    async fn a_lost_publish_is_settled_by_reading_its_commit(cx: &mut gpui::TestAppContext) {
        init(cx);
        let scenario = Scenario {
            loses_publish: true,
            ..Scenario::default()
        };
        let (session, asked) = effect_server_with(scenario, cx);
        let (view, cx) = cx.add_window_view(|window, cx| {
            EffectView::new(session, "feature".into(), "main".into(), window, cx)
        });
        cx.run_until_parked();
        let effect = view.read_with(cx, |view, _| view.effect.clone());
        effect.update(cx, |effect, cx| effect.check_merge_result(cx));
        cx.run_until_parked();
        effect.update(cx, |effect, cx| effect.publish(cx));
        cx.run_until_parked();
        effect.read_with(cx, |effect, _| {
            assert!(
                matches!(effect.merge_result(), MergeResult::Unknown { .. }),
                "{:?}",
                effect.merge_result()
            )
        });
        effect.update(cx, |effect, cx| effect.refresh(cx));
        cx.run_until_parked();
        effect.read_with(cx, |effect, _| {
            assert_eq!(
                effect.merge_result(),
                &MergeResult::Published {
                    commit: "u1".into()
                }
            )
        });
        let published = asked
            .lock()
            .unwrap()
            .iter()
            .filter(|line| line.contains("/commits/u1/publish"))
            .count();
        assert_eq!(published, 1, "never sent again");
    }

    /// The list of reads is a cached view of its own: moving through the reads draws it once a
    /// key, and drawing the tab around it for a footer change does not.
    #[gpui::test]
    async fn moving_through_the_reads_draws_the_list_once_a_key(cx: &mut gpui::TestAppContext) {
        init(cx);
        let (session, _asked) = effect_server(cx);
        let graph = session.read_with(cx, |session, _| session.overrides_key());
        cx.update(|cx| {
            studio_settings::update(cx, |settings| {
                settings.set_effect_mark(&graph, "ready", Some(EffectMark::Watched))
            })
        });
        let (view, cx) = cx.add_window_view(|window, cx| {
            EffectView::new(session, "feature".into(), "main".into(), window, cx)
        });
        cx.run_until_parked();
        let renders = |cx: &mut gpui::VisualTestContext| {
            view.read_with(cx, |view, cx| view.list.read(cx).renders)
        };
        assert!(renders(cx) > 0, "the list is drawn");
        view.update_in(cx, |view, window, cx| window.focus(&view.focus_handle, cx));
        cx.dispatch_action(NextRead);
        cx.run_until_parked();
        /// How often the list draws for one key.
        fn key<A: gpui::Action>(
            action: A,
            view: &Entity<EffectView>,
            cx: &mut gpui::VisualTestContext,
        ) -> usize {
            let renders = |cx: &mut gpui::VisualTestContext| {
                view.read_with(cx, |view, cx| view.list.read(cx).renders)
            };
            let before = renders(cx);
            cx.dispatch_action(action);
            cx.run_until_parked();
            renders(cx) - before
        }
        assert_eq!(key(PreviousRead, &view, cx), 1, "one draw a key");
        assert_eq!(
            key(PreviousRead, &view, cx),
            0,
            "a key that moves nothing draws nothing"
        );
        assert_eq!(key(NextRead, &view, cx), 1);
        assert_eq!(key(ToggleRead, &view, cx), 1);
        view.read_with(cx, |view, _| {
            assert!(view.open.contains("ready"), "the selected read unfolded")
        });
    }
}
