//! Studio's context menu (spec 002, "Menus"). Copied from Zed's
//! `crates/ui/src/components/context_menu.rs` at v1.23.2 (c0199504), Copyright Zed Industries,
//! licensed GPL-3.0-or-later as Studio is, and restyled after Tower's menus: a floating card
//! with the modal's fill, hairline and shadow, rows 26 px high inset within its corner, the
//! hovered or keyboard-highlighted row on a faint ink fill, quiet group headers and hairline
//! separators. Its behaviour — keyboard navigation, submenus, actions and accessibility — is
//! Zed's, unchanged; only the drawing is Studio's (`MenuRow`, `menu_header`, `menu_separator`
//! and the container in `render`).

// Zed's code is kept as written, so the lints Zed's workspace allows are allowed here.
#![allow(clippy::type_complexity, clippy::collapsible_if)]

use crate::ds::{elevation, radii, size, space, surface};
use gpui::{
    Action, Anchor, AnyElement, App, Bounds, DismissEvent, Entity, EventEmitter, FocusHandle,
    Focusable, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, Role,
    Size, Subscription, TaskExt, anchored, canvas, px, relative,
};
use gpui::{ClickEvent, ElementId, FontWeight, SharedString};
use menu::{SelectChild, SelectFirst, SelectLast, SelectNext, SelectParent, SelectPrevious};
use std::{
    cell::Cell,
    rc::Rc,
    time::{Duration, Instant},
};
use theme::BufferLineHeight;
use ui::{KeyBinding, prelude::*, utils::WithRemSize};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum SubmenuOpenTrigger {
    Pointer,
    Keyboard,
}

struct OpenSubmenu {
    item_index: usize,
    entity: Entity<ContextMenu>,
    trigger_bounds: Option<Bounds<Pixels>>,
    offset: Option<Pixels>,
    flip_left: bool,
    _dismiss_subscription: Subscription,
}

enum SubmenuState {
    Closed,
    Open(OpenSubmenu),
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum HoverTarget {
    #[default]
    None,
    MainMenu,
    Submenu,
}

pub enum ContextMenuItem {
    Separator,
    Header(SharedString),
    Entry(ContextMenuEntry),
    Submenu {
        label: SharedString,
        icon: Option<IconName>,
        icon_color: Option<Color>,
        builder: Rc<dyn Fn(ContextMenu, &mut Window, &mut Context<ContextMenu>) -> ContextMenu>,
    },
}

pub struct ContextMenuEntry {
    toggle: Option<(IconPosition, bool)>,
    label: SharedString,
    icon: Option<IconName>,
    icon_position: IconPosition,
    icon_color: Option<Color>,
    handler: Rc<dyn Fn(Option<&FocusHandle>, &mut Window, &mut App)>,
    action: Option<Box<dyn Action>>,
    disabled: bool,
}

impl ContextMenuEntry {
    pub fn new(label: impl Into<SharedString>) -> Self {
        ContextMenuEntry {
            toggle: None,
            label: label.into(),
            icon: None,
            icon_position: IconPosition::Start,
            icon_color: None,
            handler: Rc::new(|_, _, _| {}),
            action: None,
            disabled: false,
        }
    }

    pub fn toggleable(mut self, toggle_position: IconPosition, toggled: bool) -> Self {
        self.toggle = Some((toggle_position, toggled));
        self
    }

    pub fn icon(mut self, icon: IconName) -> Self {
        self.icon = Some(icon);
        self
    }

    pub fn icon_color(mut self, icon_color: Color) -> Self {
        self.icon_color = Some(icon_color);
        self
    }

    pub fn action(mut self, action: Box<dyn Action>) -> Self {
        self.action = Some(action);
        self
    }

    pub fn handler(mut self, handler: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.handler = Rc::new(move |_, window, cx| handler(window, cx));
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

impl FluentBuilder for ContextMenuEntry {}

impl From<ContextMenuEntry> for ContextMenuItem {
    fn from(entry: ContextMenuEntry) -> Self {
        ContextMenuItem::Entry(entry)
    }
}

pub struct ContextMenu {
    builder: Option<Rc<dyn Fn(Self, &mut Window, &mut Context<Self>) -> Self>>,
    items: Vec<ContextMenuItem>,
    focus_handle: FocusHandle,
    action_context: Option<FocusHandle>,
    selected_index: Option<usize>,
    delayed: bool,
    clicked: bool,
    _on_blur_subscription: Subscription,
    keep_open_on_confirm: bool,
    main_menu: Option<Entity<ContextMenu>>,
    main_menu_observed_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    // Submenu-related fields
    submenu_state: SubmenuState,
    hover_target: HoverTarget,
    submenu_safety_threshold_x: Option<Pixels>,
    submenu_trigger_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    submenu_trigger_mouse_down: bool,
    ignore_blur_until: Option<Instant>,
    /// When set to true, the next on_focus_in callback will not automatically
    /// select an item. This prevents a visual flash where a submenu close in
    /// on_hover(false) returns focus to the main menu and on_focus_in
    /// re-selects the first item before the next on_hover(true) clears it.
    /// Always true when accessibility support is disabled.
    suppress_focus_selection: bool,
}

impl Focusable for ContextMenu {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for ContextMenu {}

impl FluentBuilder for ContextMenu {}

impl ContextMenu {
    #[inline(always)]
    pub fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        build_menu: impl FnOnce(Self, &mut Window, &mut Context<Self>) -> Self,
    ) -> Self {
        let menu = Self::new_inner(window, cx);
        build_menu(menu, window, cx)
    }

    pub fn build(
        window: &mut Window,
        cx: &mut App,
        f: impl FnOnce(Self, &mut Window, &mut Context<Self>) -> Self,
    ) -> Entity<Self> {
        cx.new(|cx| Self::new(window, cx, f))
    }

    /// Builds a [`ContextMenu`] that will stay open when making changes instead of closing after each confirmation.
    ///
    /// The main difference from [`ContextMenu::build`] is the type of the `builder`, as we need to be able to hold onto
    /// it to call it again.
    pub fn build_persistent(
        window: &mut Window,
        cx: &mut App,
        builder: impl Fn(Self, &mut Window, &mut Context<Self>) -> Self + 'static,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let builder = Rc::new(builder);
            let mut menu = Self::new_inner(window, cx);
            menu.builder = Some(builder.clone());
            menu.keep_open_on_confirm = true;
            builder(menu, window, cx)
        })
    }

    /// Rebuilds the menu.
    ///
    /// This is used to refresh the menu entries when entries are toggled when the menu is configured with
    /// `keep_open_on_confirm = true`.
    ///
    /// This only works if the [`ContextMenu`] was constructed using [`ContextMenu::build_persistent`]. Otherwise it is
    /// a no-op.
    pub fn rebuild(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(builder) = self.builder.clone() else {
            return;
        };

        // The way we rebuild the menu is a bit of a hack.
        let new_menu = builder(Self::empty(None, window, cx), window, cx);

        self.items = new_menu.items;

        cx.notify();
    }

    pub fn context(mut self, focus: FocusHandle) -> Self {
        self.action_context = Some(focus);
        self
    }

    pub fn header(mut self, title: impl Into<SharedString>) -> Self {
        self.items.push(ContextMenuItem::Header(title.into()));
        self
    }

    pub fn separator(mut self) -> Self {
        self.items.push(ContextMenuItem::Separator);
        self
    }

    pub fn item(mut self, item: impl Into<ContextMenuItem>) -> Self {
        self.items.push(item.into());
        self
    }

    pub fn entry(
        mut self,
        label: impl Into<SharedString>,
        action: Option<Box<dyn Action>>,
        handler: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        self.items.push(ContextMenuItem::Entry(ContextMenuEntry {
            toggle: None,
            label: label.into(),
            handler: Rc::new(move |_, window, cx| handler(window, cx)),
            icon: None,
            icon_position: IconPosition::End,
            icon_color: None,
            action,
            disabled: false,
        }));
        self
    }

    pub fn toggleable_entry(
        mut self,
        label: impl Into<SharedString>,
        toggled: bool,
        position: IconPosition,
        action: Option<Box<dyn Action>>,
        handler: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        self.items.push(ContextMenuItem::Entry(ContextMenuEntry {
            toggle: Some((position, toggled)),
            label: label.into(),
            handler: Rc::new(move |_, window, cx| handler(window, cx)),
            icon: None,
            icon_position: position,
            icon_color: None,
            action,
            disabled: false,
        }));
        self
    }

    pub fn action(mut self, label: impl Into<SharedString>, action: Box<dyn Action>) -> Self {
        self.items.push(ContextMenuItem::Entry(ContextMenuEntry {
            toggle: None,
            label: label.into(),
            action: Some(action.boxed_clone()),
            handler: Rc::new(move |context, window, cx| {
                if let Some(context) = &context {
                    window.focus(context, cx);
                }
                window.dispatch_action(action.boxed_clone(), cx);
            }),
            icon: None,
            icon_position: IconPosition::End,
            icon_color: None,
            disabled: false,
        }));
        self
    }

    pub fn submenu(
        mut self,
        label: impl Into<SharedString>,
        builder: impl Fn(ContextMenu, &mut Window, &mut Context<ContextMenu>) -> ContextMenu + 'static,
    ) -> Self {
        self.items.push(ContextMenuItem::Submenu {
            label: label.into(),
            icon: None,
            icon_color: None,
            builder: Rc::new(builder),
        });
        self
    }

    pub fn submenu_with_colored_icon(
        mut self,
        label: impl Into<SharedString>,
        icon: IconName,
        icon_color: Color,
        builder: impl Fn(ContextMenu, &mut Window, &mut Context<ContextMenu>) -> ContextMenu + 'static,
    ) -> Self {
        self.items.push(ContextMenuItem::Submenu {
            label: label.into(),
            icon: Some(icon),
            icon_color: Some(icon_color),
            builder: Rc::new(builder),
        });
        self
    }

    pub fn keep_open_on_confirm(mut self, keep_open: bool) -> Self {
        self.keep_open_on_confirm = keep_open;
        self
    }

    pub fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.selected_index else {
            return cx.emit(DismissEvent);
        };

        if let Some(ContextMenuItem::Submenu { builder, .. }) = self.items.get(ix) {
            self.open_submenu(
                ix,
                builder.clone(),
                SubmenuOpenTrigger::Keyboard,
                window,
                cx,
            );

            if let SubmenuState::Open(open_submenu) = &self.submenu_state {
                let focus_handle = open_submenu.entity.read(cx).focus_handle.clone();
                window.focus(&focus_handle, cx);
                open_submenu.entity.update(cx, |submenu, cx| {
                    submenu.select_first(&SelectFirst, window, cx);
                });
            }

            cx.notify();
            return;
        }

        let context = self.action_context.as_ref();

        if let Some(ContextMenuItem::Entry(ContextMenuEntry {
            handler,
            disabled: false,
            ..
        })) = self.items.get(ix)
        {
            (handler)(context, window, cx)
        }

        if self.main_menu.is_some() && !self.keep_open_on_confirm {
            self.clicked = true;
        }

        if self.keep_open_on_confirm {
            self.rebuild(window, cx);
        } else {
            cx.emit(DismissEvent);
        }
    }

    /// Confirms the selected item as [`Self::confirm`] does; with nothing selected the menu stays
    /// open.
    pub fn secondary_confirm(
        &mut self,
        _: &menu::SecondaryConfirm,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected_index.is_some() {
            self.confirm(&menu::Confirm, window, cx);
        }
    }

    pub fn cancel(&mut self, _: &menu::Cancel, window: &mut Window, cx: &mut Context<Self>) {
        if self.main_menu.is_some() {
            cx.emit(DismissEvent);

            // Restore keyboard focus to the parent menu so arrow keys / Escape / Enter work again.
            if let Some(parent) = &self.main_menu {
                let parent_focus = parent.read(cx).focus_handle.clone();

                parent.update(cx, |parent, _cx| {
                    parent.ignore_blur_until = Some(Instant::now() + Duration::from_millis(200));
                });

                window.focus(&parent_focus, cx);
            }

            return;
        }

        cx.emit(DismissEvent);
    }

    pub fn clear_selected(&mut self) {
        self.selected_index = None;
    }

    pub fn select_first(&mut self, _: &SelectFirst, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.items.iter().position(|item| item.is_selectable()) {
            self.select_index(ix, window, cx);
        }
        cx.notify();
    }

    /// Selects the currently-checked entry if one exists (e.g. the active value
    /// in a single-select dropdown), otherwise the first selectable item.
    ///
    /// This is intended to be called when the menu opens. Per the ARIA menu
    /// button pattern, opening a menu should place focus on a menu item rather
    /// than the menu container, so that assistive technology immediately
    /// announces a meaningful item (ideally the current selection) instead of
    /// just "menu".
    pub fn select_toggled_or_first(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let toggled_ix = self.items.iter().position(|item| {
            matches!(
                item,
                ContextMenuItem::Entry(ContextMenuEntry {
                    toggle: Some((_, true)),
                    ..
                })
            )
        });
        if let Some(ix) = toggled_ix {
            self.select_index(ix, window, cx);
            cx.notify();
        } else {
            self.select_first(&SelectFirst, window, cx);
        }
    }

    pub fn select_last(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<usize> {
        for (ix, item) in self.items.iter().enumerate().rev() {
            if item.is_selectable() {
                return self.select_index(ix, window, cx);
            }
        }
        None
    }

    fn handle_select_last(&mut self, _: &SelectLast, window: &mut Window, cx: &mut Context<Self>) {
        if self.select_last(window, cx).is_some() {
            cx.notify();
        }
    }

    pub fn select_next(&mut self, _: &SelectNext, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.selected_index {
            let next_index = ix + 1;
            if self.items.len() <= next_index {
                self.select_first(&SelectFirst, window, cx);
                return;
            } else {
                for (ix, item) in self.items.iter().enumerate().skip(next_index) {
                    if item.is_selectable() {
                        self.select_index(ix, window, cx);
                        cx.notify();
                        return;
                    }
                }
            }
        }
        self.select_first(&SelectFirst, window, cx);
    }

    pub fn select_previous(
        &mut self,
        _: &SelectPrevious,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(ix) = self.selected_index {
            for (ix, item) in self.items.iter().enumerate().take(ix).rev() {
                if item.is_selectable() {
                    self.select_index(ix, window, cx);
                    cx.notify();
                    return;
                }
            }
        }
        self.handle_select_last(&SelectLast, window, cx);
    }

    pub fn select_submenu_child(
        &mut self,
        _: &SelectChild,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.selected_index else {
            return;
        };

        let Some(ContextMenuItem::Submenu { builder, .. }) = self.items.get(ix) else {
            return;
        };

        self.open_submenu(
            ix,
            builder.clone(),
            SubmenuOpenTrigger::Keyboard,
            window,
            cx,
        );

        if let SubmenuState::Open(open_submenu) = &self.submenu_state {
            let focus_handle = open_submenu.entity.read(cx).focus_handle.clone();
            window.focus(&focus_handle, cx);
            open_submenu.entity.update(cx, |submenu, cx| {
                submenu.select_first(&SelectFirst, window, cx);
            });
        }

        cx.notify();
    }

    pub fn select_submenu_parent(
        &mut self,
        _: &SelectParent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.main_menu.is_none() {
            return;
        }

        if let Some(parent) = &self.main_menu {
            let parent_clone = parent.clone();

            let parent_focus = parent.read(cx).focus_handle.clone();
            window.focus(&parent_focus, cx);

            cx.emit(DismissEvent);

            parent_clone.update(cx, |parent, cx| {
                if let SubmenuState::Open(open_submenu) = &parent.submenu_state {
                    let trigger_index = open_submenu.item_index;
                    parent.close_submenu(false, cx);
                    let _ = parent.select_index(trigger_index, window, cx);
                    cx.notify();
                }
            });

            return;
        }

        cx.emit(DismissEvent);
    }

    fn select_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let item = self.items.get(ix)?;
        if item.is_selectable() {
            self.selected_index = Some(ix);
        }
        Some(ix)
    }

    fn create_submenu(
        builder: Rc<dyn Fn(ContextMenu, &mut Window, &mut Context<ContextMenu>) -> ContextMenu>,
        parent_entity: Entity<ContextMenu>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (Entity<ContextMenu>, Subscription) {
        let submenu = Self::build_submenu(builder, parent_entity, window, cx);

        let dismiss_subscription = cx.subscribe(&submenu, |this, submenu, _: &DismissEvent, cx| {
            let should_dismiss_parent = submenu.read(cx).clicked;

            this.close_submenu(false, cx);

            if should_dismiss_parent {
                cx.emit(DismissEvent);
            }
        });

        (submenu, dismiss_subscription)
    }

    fn build_submenu(
        builder: Rc<dyn Fn(ContextMenu, &mut Window, &mut Context<ContextMenu>) -> ContextMenu>,
        parent_entity: Entity<ContextMenu>,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<ContextMenu> {
        cx.new(|cx| {
            let menu = Self::empty(Some(parent_entity), window, cx);
            builder(menu, window, cx)
        })
    }

    fn close_submenu(&mut self, clear_selection: bool, cx: &mut Context<Self>) {
        self.submenu_state = SubmenuState::Closed;
        self.hover_target = HoverTarget::MainMenu;
        self.submenu_safety_threshold_x = None;
        self.main_menu_observed_bounds.set(None);
        self.submenu_trigger_bounds.set(None);

        if clear_selection {
            self.selected_index = None;
        }

        cx.notify();
    }

    fn open_submenu(
        &mut self,
        item_index: usize,
        builder: Rc<dyn Fn(ContextMenu, &mut Window, &mut Context<ContextMenu>) -> ContextMenu>,
        reason: SubmenuOpenTrigger,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // If the submenu is already open for this item, don't recreate it.
        if matches!(
            &self.submenu_state,
            SubmenuState::Open(open_submenu) if open_submenu.item_index == item_index
        ) {
            return;
        }

        let (submenu, dismiss_subscription) =
            Self::create_submenu(builder, cx.entity(), window, cx);

        let flip_left = self
            .main_menu_observed_bounds
            .get()
            .is_some_and(|bounds| bounds.right() + px(200.0) > window.viewport_size().width);

        // If we're switching from one submenu item to another, throw away any previously-captured
        // offset so we don't reuse a stale position.
        self.main_menu_observed_bounds.set(None);
        self.submenu_trigger_bounds.set(None);

        self.submenu_safety_threshold_x = None;
        self.hover_target = HoverTarget::MainMenu;

        // When opening a submenu via keyboard, there is a brief moment where focus/hover can
        // transition in a way that triggers the parent menu's `on_blur` dismissal.
        if matches!(reason, SubmenuOpenTrigger::Keyboard) {
            self.ignore_blur_until = Some(Instant::now() + Duration::from_millis(150));
        }

        let trigger_bounds = self.submenu_trigger_bounds.get();

        self.submenu_state = SubmenuState::Open(OpenSubmenu {
            item_index,
            entity: submenu,
            trigger_bounds,
            offset: None,
            flip_left,
            _dismiss_subscription: dismiss_subscription,
        });

        cx.notify();
    }

    pub fn on_action_dispatch(
        &mut self,
        dispatched: &dyn Action,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.clicked {
            cx.propagate();
            return;
        }

        if let Some(ix) = self.items.iter().position(|item| {
            if let ContextMenuItem::Entry(ContextMenuEntry {
                action: Some(action),
                disabled: false,
                ..
            }) = item
            {
                action.partial_eq(dispatched)
            } else {
                false
            }
        }) {
            self.select_index(ix, window, cx);
            self.delayed = true;
            cx.notify();
            let action = dispatched.boxed_clone();
            cx.spawn_in(window, async move |this, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(50))
                    .await;
                cx.update(|window, cx| {
                    this.update(cx, |this, cx| {
                        this.cancel(&menu::Cancel, window, cx);
                        window.dispatch_action(action, cx);
                    })
                })
            })
            .detach_and_log_err(cx);
        } else {
            cx.propagate()
        }
    }

    fn render_menu_item(
        &self,
        ix: usize,
        item: &ContextMenuItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        // The menu keeps real focus on its container, so for assistive
        // technology to track the selected item we report it as the active
        // descendant. GPUI only honors this while the menu actually holds
        // focus, so we mark the selected item unconditionally here.
        let is_active_descendant = Some(ix) == self.selected_index;
        match item {
            ContextMenuItem::Separator => menu_separator(cx).into_any_element(),
            ContextMenuItem::Header(header) => menu_header(header.clone()).into_any_element(),
            ContextMenuItem::Entry(entry) => self
                .render_menu_entry(ix, entry, is_active_descendant, window, cx)
                .into_any_element(),
            ContextMenuItem::Submenu {
                label,
                icon,
                icon_color,
                ..
            } => self
                .render_submenu_item_trigger(
                    ix,
                    label.clone(),
                    *icon,
                    *icon_color,
                    is_active_descendant,
                    cx,
                )
                .into_any_element(),
        }
    }

    fn render_submenu_item_trigger(
        &self,
        ix: usize,
        label: SharedString,
        icon: Option<IconName>,
        icon_color: Option<Color>,
        is_active_descendant: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let toggle_state = Some(ix) == self.selected_index
            || matches!(
                &self.submenu_state,
                SubmenuState::Open(open_submenu) if open_submenu.item_index == ix
            );

        div()
            .id(("context-menu-submenu-trigger", ix))
            .capture_any_mouse_down(cx.listener(move |this, event: &MouseDownEvent, _, _| {
                // This prevents on_hover(false) from closing the submenu during a click.
                if event.button == MouseButton::Left {
                    this.submenu_trigger_mouse_down = true;
                }
            }))
            .capture_any_mouse_up(cx.listener(move |this, event: &MouseUpEvent, _, _| {
                if event.button == MouseButton::Left {
                    this.submenu_trigger_mouse_down = false;
                }
            }))
            .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
                if matches!(&this.submenu_state, SubmenuState::Open(_))
                    || this.selected_index == Some(ix)
                {
                    this.submenu_safety_threshold_x = Some(event.position.x - px(100.0));
                }

                cx.notify();
            }))
            .child(
                MenuRow::new(ix)
                    .aria_role(Role::MenuItem)
                    .when(is_active_descendant, |item| item.aria_active_descendant())
                    .aria_label(label.clone())
                    .toggle_state(toggle_state)
                    .child(
                        canvas(
                            {
                                let trigger_bounds_cell = self.submenu_trigger_bounds.clone();
                                move |bounds, _window, _cx| {
                                    if toggle_state {
                                        trigger_bounds_cell.set(Some(bounds));
                                    }
                                }
                            },
                            |_bounds, _state, _window, _cx| {},
                        )
                        .size_full()
                        .absolute()
                        .top_0()
                        .left_0(),
                    )
                    .on_hover(cx.listener(move |this, hovered, window, cx| {
                        let mouse_pos = window.mouse_position();

                        if *hovered {
                            this.clear_selected();
                            this.suppress_focus_selection = true;
                            window.focus(&this.focus_handle.clone(), cx);
                            this.hover_target = HoverTarget::MainMenu;
                            this.submenu_safety_threshold_x = Some(mouse_pos.x - px(50.0));

                            if let Some(ContextMenuItem::Submenu { builder, .. }) =
                                this.items.get(ix)
                            {
                                this.open_submenu(
                                    ix,
                                    builder.clone(),
                                    SubmenuOpenTrigger::Pointer,
                                    window,
                                    cx,
                                );
                            }

                            cx.notify();
                        } else {
                            if this.submenu_trigger_mouse_down {
                                return;
                            }

                            let is_open_for_this_item = matches!(
                                &this.submenu_state,
                                SubmenuState::Open(open_submenu) if open_submenu.item_index == ix
                            );

                            let mouse_in_submenu_zone = this
                                .padded_submenu_bounds()
                                .is_some_and(|bounds| bounds.contains(&window.mouse_position()));

                            if is_open_for_this_item
                                && this.hover_target != HoverTarget::Submenu
                                && !mouse_in_submenu_zone
                            {
                                this.close_submenu(false, cx);
                                this.clear_selected();
                                this.suppress_focus_selection = true;
                                window.focus(&this.focus_handle.clone(), cx);
                                cx.notify();
                            }
                        }
                    }))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if matches!(
                            &this.submenu_state,
                            SubmenuState::Open(open_submenu) if open_submenu.item_index == ix
                        ) {
                            return;
                        }

                        if let Some(ContextMenuItem::Submenu { builder, .. }) = this.items.get(ix) {
                            this.open_submenu(
                                ix,
                                builder.clone(),
                                SubmenuOpenTrigger::Pointer,
                                window,
                                cx,
                            );
                        }
                    }))
                    .child(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .justify_between()
                            .child(
                                h_flex()
                                    .gap_1p5()
                                    .when_some(icon, |this, icon_name| {
                                        this.child(
                                            Icon::new(icon_name)
                                                .size(IconSize::Small)
                                                .color(icon_color.unwrap_or(Color::Muted)),
                                        )
                                    })
                                    .child(Label::new(label).color(Color::Default)),
                            )
                            .child(
                                Icon::new(IconName::ChevronRight)
                                    .size(IconSize::Small)
                                    .color(Color::Muted),
                            ),
                    ),
            )
    }

    fn padded_submenu_bounds(&self) -> Option<Bounds<Pixels>> {
        let bounds = self.main_menu_observed_bounds.get()?;
        Some(Bounds {
            origin: Point {
                x: bounds.origin.x - px(50.0),
                y: bounds.origin.y - px(50.0),
            },
            size: Size {
                width: bounds.size.width + px(100.0),
                height: bounds.size.height + px(100.0),
            },
        })
    }

    fn render_submenu_container(
        &self,
        ix: usize,
        submenu: Entity<ContextMenu>,
        offset: Pixels,
        flip_left: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let bounds_cell = self.main_menu_observed_bounds.clone();
        let canvas = canvas(
            {
                move |bounds, _window, _cx| {
                    bounds_cell.set(Some(bounds));
                }
            },
            |_bounds, _state, _window, _cx| {},
        )
        .size_full()
        .absolute()
        .top_0()
        .left_0();

        div()
            .id(("submenu-container", ix))
            .absolute()
            .top(offset)
            .when(flip_left, |this| this.right_full().mr_neg_0p5())
            .when(!flip_left, |this| this.left_full().ml_neg_0p5())
            .on_hover(cx.listener(|this, hovered, _, _| {
                if *hovered {
                    this.hover_target = HoverTarget::Submenu;
                }
            }))
            .child(
                anchored()
                    .anchor(if flip_left {
                        Anchor::TopRight
                    } else {
                        Anchor::TopLeft
                    })
                    .snap_to_window_with_margin(px(8.0))
                    .child(
                        div()
                            .id(("submenu-hover-zone", ix))
                            .occlude()
                            .child(canvas)
                            .child(submenu),
                    ),
            )
    }

    fn render_menu_entry(
        &self,
        ix: usize,
        entry: &ContextMenuEntry,
        is_active_descendant: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let ContextMenuEntry {
            toggle,
            label,
            handler,
            icon,
            icon_position,
            icon_color,
            action,
            disabled,
        } = entry;
        // Report the item's keyboard shortcut to assistive technology, resolving
        // the action's binding the same way the visible accelerator (rendered
        // below) is.
        let keyboard_shortcut = action.as_ref().and_then(|action| {
            let binding = self
                .action_context
                .as_ref()
                .map(|focus| KeyBinding::for_action_in(&**action, focus, cx))
                .unwrap_or_else(|| KeyBinding::for_action(&**action, cx));
            binding.keyboard_shortcut_text(window, cx)
        });

        let handler = handler.clone();
        let menu = cx.entity().downgrade();

        let icon_color = if *disabled {
            Color::Muted
        } else if toggle.is_some() {
            icon_color.unwrap_or(Color::Accent)
        } else {
            icon_color.unwrap_or(Color::Default)
        };

        let label_color = if *disabled {
            Color::Disabled
        } else {
            Color::Default
        };

        let label_element = if let Some(icon_name) = icon {
            h_flex()
                .gap(px(space::MD))
                .when(
                    *icon_position == IconPosition::Start && toggle.is_none(),
                    |flex| {
                        flex.child(
                            Icon::new(*icon_name)
                                .size(IconSize::Small)
                                .color(icon_color),
                        )
                    },
                )
                .child(Label::new(label.clone()).color(label_color).truncate())
                .when(*icon_position == IconPosition::End, |flex| {
                    flex.child(
                        Icon::new(*icon_name)
                            .size(IconSize::Small)
                            .color(icon_color),
                    )
                })
                .into_any_element()
        } else {
            Label::new(label.clone())
                .color(label_color)
                .truncate()
                .into_any_element()
        };

        div()
            .id(("context-menu-child", ix))
            .child(
                MenuRow::new(ix)
                    .disabled(*disabled)
                    .aria_role(if toggle.is_some() {
                        Role::MenuItemCheckBox
                    } else {
                        Role::MenuItem
                    })
                    .when_some(*toggle, |item, (_, checked)| item.aria_checked(checked))
                    .when(is_active_descendant, |item| item.aria_active_descendant())
                    .aria_label(label.clone())
                    .when_some(keyboard_shortcut, |item, keyboard_shortcut| {
                        item.aria_keyshortcuts(keyboard_shortcut)
                    })
                    .toggle_state(Some(ix) == self.selected_index)
                    .when(self.main_menu.is_none() && !*disabled, |item| {
                        item.on_hover(cx.listener(move |this, hovered, window, cx| {
                            if *hovered {
                                this.clear_selected();
                                this.suppress_focus_selection = true;
                                window.focus(&this.focus_handle.clone(), cx);

                                if let SubmenuState::Open(open_submenu) = &this.submenu_state {
                                    if open_submenu.item_index != ix {
                                        this.close_submenu(false, cx);
                                        cx.notify();
                                    }
                                }
                            }
                        }))
                    })
                    .when(self.main_menu.is_some(), |item| {
                        item.on_click(cx.listener(move |this, _, window, cx| {
                            if matches!(
                                &this.submenu_state,
                                SubmenuState::Open(open_submenu) if open_submenu.item_index == ix
                            ) {
                                return;
                            }

                            if let Some(ContextMenuItem::Submenu { builder, .. }) =
                                this.items.get(ix)
                            {
                                this.open_submenu(
                                    ix,
                                    builder.clone(),
                                    SubmenuOpenTrigger::Pointer,
                                    window,
                                    cx,
                                );
                            }
                        }))
                        .on_hover(cx.listener(
                            move |this, hovered, window, cx| {
                                if *hovered {
                                    this.clear_selected();
                                    cx.notify();
                                }

                                if let Some(parent) = &this.main_menu {
                                    let mouse_pos = window.mouse_position();
                                    let parent_clone = parent.clone();

                                    if *hovered {
                                        parent.update(cx, |parent, _| {
                                            parent.clear_selected();
                                            parent.hover_target = HoverTarget::Submenu;
                                        });
                                    } else {
                                        parent_clone.update(cx, |parent, cx| {
                                            if matches!(
                                                &parent.submenu_state,
                                                SubmenuState::Open(_)
                                            ) {
                                                // Only close if mouse is to the left of the safety threshold
                                                // (prevents accidental close when moving diagonally toward submenu)
                                                let should_close = parent
                                                    .submenu_safety_threshold_x
                                                    .map(|threshold_x| mouse_pos.x < threshold_x)
                                                    .unwrap_or(true);

                                                if should_close {
                                                    parent.close_submenu(true, cx);
                                                }
                                            }
                                        });
                                    }
                                }
                            },
                        ))
                    })
                    .when_some(*toggle, |list_item, (position, toggled)| {
                        let contents = div()
                            .flex_none()
                            .child(
                                Icon::new(icon.unwrap_or(IconName::Check))
                                    .color(icon_color)
                                    .size(IconSize::Small),
                            )
                            .when(!toggled, |contents| contents.invisible());

                        match position {
                            IconPosition::Start => list_item.start_slot(contents),
                            IconPosition::End => list_item.end_slot(contents),
                        }
                    })
                    .child(
                        h_flex()
                            .w_full()
                            .justify_between()
                            .child(label_element)
                            .debug_selector(|| format!("MENU_ITEM-{}", label))
                            .children(action.as_ref().map(|action| {
                                let binding = self
                                    .action_context
                                    .as_ref()
                                    .map(|focus| KeyBinding::for_action_in(&**action, focus, cx))
                                    .unwrap_or_else(|| KeyBinding::for_action(&**action, cx));

                                div().ml_4().child(binding.disabled(*disabled))
                            })),
                    )
                    .on_click({
                        let context = self.action_context.clone();
                        let keep_open_on_confirm = self.keep_open_on_confirm;
                        move |_, window, cx| {
                            handler(context.as_ref(), window, cx);
                            menu.update(cx, |menu, cx| {
                                menu.clicked = true;
                                if keep_open_on_confirm {
                                    menu.rebuild(window, cx);
                                } else {
                                    cx.emit(DismissEvent);
                                }
                            })
                            .ok();
                        }
                    }),
            )
            .into_any_element()
    }

    #[inline(never)]
    fn new_inner(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let menu = Self::empty(None, window, cx);
        window.refresh();

        if window.is_a11y_enabled() {
            // When the menu first receives focus (i.e. when it opens), move the
            // selection onto a menu item so assistive technology announces a real
            // item rather than the bare menu container. Per the ARIA menu button
            // pattern, opening a menu places focus on a menu item; for select-style
            // menus we prefer the currently-checked item. We only do this when
            // nothing is selected yet so we don't override an existing selection.
            cx.on_focus_in(&menu.focus_handle, window, |context_menu, window, cx| {
                if context_menu.selected_index.is_none() && !context_menu.suppress_focus_selection {
                    context_menu.select_toggled_or_first(window, cx);
                }
                context_menu.suppress_focus_selection = false;
            })
            .detach();
        }

        menu
    }

    /// An empty menu with its own focus. A main menu dismisses itself when it loses focus,
    /// unless the focus moved into its open submenu; a submenu (one with `main_menu`) is closed
    /// by its parent instead.
    fn empty(
        main_menu: Option<Entity<ContextMenu>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let _on_blur_subscription = cx.on_blur(
            &focus_handle,
            window,
            |context_menu: &mut ContextMenu, window, cx| {
                if context_menu.main_menu.is_some() {
                    return;
                }

                if let Some(ignore_until) = context_menu.ignore_blur_until {
                    if Instant::now() < ignore_until {
                        return;
                    } else {
                        context_menu.ignore_blur_until = None;
                    }
                }

                if let SubmenuState::Open(open_submenu) = &context_menu.submenu_state {
                    let submenu_focus = open_submenu.entity.read(cx).focus_handle.clone();
                    if submenu_focus.contains_focused(window, cx) {
                        return;
                    }
                }

                context_menu.cancel(&menu::Cancel, window, cx)
            },
        );

        Self {
            builder: None,
            items: Vec::new(),
            focus_handle,
            action_context: None,
            selected_index: None,
            delayed: false,
            clicked: false,
            _on_blur_subscription,
            keep_open_on_confirm: false,
            main_menu,
            main_menu_observed_bounds: Rc::new(Cell::new(None)),
            submenu_state: SubmenuState::Closed,
            hover_target: HoverTarget::MainMenu,
            submenu_safety_threshold_x: None,
            submenu_trigger_bounds: Rc::new(Cell::new(None)),
            submenu_trigger_mouse_down: false,
            ignore_blur_until: None,
            suppress_focus_selection: !window.is_a11y_enabled(),
        }
    }
}

impl ContextMenuItem {
    fn is_selectable(&self) -> bool {
        match self {
            ContextMenuItem::Header(_) | ContextMenuItem::Separator => false,
            ContextMenuItem::Entry(ContextMenuEntry { disabled, .. }) => !disabled,
            ContextMenuItem::Submenu { .. } => true,
        }
    }
}

impl Render for ContextMenu {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme_settings = theme::theme_settings(cx);
        let ui_font_size = theme_settings.ui_font_size(cx);
        let ui_font_family = theme_settings.ui_font(cx).family.clone();
        // Menus can be deferred from inside elements that override the text
        // style (e.g. the editor with a custom `buffer_line_height`), so always
        // apply the default line height to render the same everywhere.
        let line_height = relative(BufferLineHeight::Comfortable.value());
        let window_size = window.viewport_size();
        let rem_size = window.rem_size();
        let is_wide_window = window_size.width / rem_size > rems_from_px(800_f32).0;

        let mut focus_submenu: Option<FocusHandle> = None;

        let submenu_container = match &mut self.submenu_state {
            SubmenuState::Open(open_submenu) => {
                let is_initializing = open_submenu.offset.is_none();

                let computed_offset = if is_initializing {
                    let menu_bounds = self.main_menu_observed_bounds.get();
                    let trigger_bounds = open_submenu
                        .trigger_bounds
                        .or_else(|| self.submenu_trigger_bounds.get());

                    match (menu_bounds, trigger_bounds) {
                        (Some(menu_bounds), Some(trigger_bounds)) => {
                            Some(trigger_bounds.origin.y - menu_bounds.origin.y)
                        }
                        _ => None,
                    }
                } else {
                    None
                };

                if let Some(offset) = open_submenu.offset.or(computed_offset) {
                    if open_submenu.offset.is_none() {
                        open_submenu.offset = Some(offset);
                    }

                    focus_submenu = Some(open_submenu.entity.read(cx).focus_handle.clone());
                    Some((
                        open_submenu.item_index,
                        open_submenu.entity.clone(),
                        offset,
                        open_submenu.flip_left,
                    ))
                } else {
                    None
                }
            }
            _ => None,
        };

        let render_menu = |cx: &mut Context<Self>, window: &mut Window| {
            let bounds_cell = self.main_menu_observed_bounds.clone();
            let menu_bounds_measure = canvas(
                {
                    move |bounds, _window, _cx| {
                        bounds_cell.set(Some(bounds));
                    }
                },
                |_bounds, _state, _window, _cx| {},
            )
            .size_full()
            .absolute()
            .top_0()
            .left_0();

            WithRemSize::new(ui_font_size)
                .occlude()
                .font_family(ui_font_family.clone())
                .line_height(line_height)
                .map(|this| menu_surface(this, cx))
                .flex()
                .flex_row()
                .flex_shrink_0()
                .child(
                    v_flex()
                        .id("context-menu")
                        .role(Role::Menu)
                        .max_h(vh(0.75, window))
                        .flex_shrink_0()
                        .child(menu_bounds_measure)
                        .min_w(px(size::MENU_MIN_WIDTH))
                        .flex_1()
                        .overflow_y_scroll()
                        .track_focus(&self.focus_handle(cx))
                        .key_context("menu")
                        .on_action(cx.listener(ContextMenu::select_first))
                        .on_action(cx.listener(ContextMenu::handle_select_last))
                        .on_action(cx.listener(ContextMenu::select_next))
                        .on_action(cx.listener(ContextMenu::select_previous))
                        .on_action(cx.listener(ContextMenu::select_submenu_child))
                        .on_action(cx.listener(ContextMenu::select_submenu_parent))
                        .on_action(cx.listener(ContextMenu::confirm))
                        .on_action(cx.listener(ContextMenu::secondary_confirm))
                        .on_action(cx.listener(ContextMenu::cancel))
                        .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                            if *hovered {
                                this.hover_target = HoverTarget::MainMenu;
                                if let Some(parent) = &this.main_menu {
                                    parent.update(cx, |parent, _| {
                                        parent.hover_target = HoverTarget::Submenu;
                                    });
                                }
                            }
                        }))
                        .on_mouse_down_out(cx.listener(
                            |this, event: &MouseDownEvent, window, cx| {
                                if matches!(&this.submenu_state, SubmenuState::Open(_)) {
                                    if let Some(padded_bounds) = this.padded_submenu_bounds() {
                                        if padded_bounds.contains(&event.position) {
                                            return;
                                        }
                                    }
                                }

                                if let Some(parent) = &this.main_menu {
                                    let overridden_by_parent_trigger = parent
                                        .read(cx)
                                        .submenu_trigger_bounds
                                        .get()
                                        .is_some_and(|bounds| bounds.contains(&event.position));
                                    if overridden_by_parent_trigger {
                                        return;
                                    }
                                }

                                this.cancel(&menu::Cancel, window, cx)
                            },
                        ))
                        .when(!self.delayed, |mut el| {
                            for item in self.items.iter() {
                                if let ContextMenuItem::Entry(ContextMenuEntry {
                                    action: Some(action),
                                    disabled: false,
                                    ..
                                }) = item
                                {
                                    el = el.on_boxed_action(
                                        &**action,
                                        cx.listener(ContextMenu::on_action_dispatch),
                                    );
                                }
                            }
                            el
                        })
                        .child(
                            v_flex().w_full().p(px(space::XS)).children(
                                self.items
                                    .iter()
                                    .enumerate()
                                    .map(|(ix, item)| self.render_menu_item(ix, item, window, cx)),
                            ),
                        ),
                )
        };

        if let Some(focus_handle) = focus_submenu.as_ref() {
            window.focus(focus_handle, cx);
        }

        let container = if is_wide_window {
            div().relative()
        } else {
            v_flex().w_full().relative().gap_1().justify_end()
        };
        container.child(render_menu(cx, window)).when_some(
            submenu_container,
            |this, (ix, submenu, offset, flip_left)| {
                this.child(self.render_submenu_container(ix, submenu, offset, flip_left, cx))
            },
        )
    }
}

/// The menu's card: the floating fill, its hairline, the floating shadow and the menu radius.
pub fn menu_surface<E: Styled>(element: E, cx: &App) -> E {
    element
        .bg(surface::floating(cx))
        .border_1()
        .border_color(surface::floating_border(cx))
        .rounded(px(radii(cx).menu()))
        .shadow(elevation::floating(cx))
}

/// A hairline between groups of rows, edge to edge of the card.
pub fn menu_separator(cx: &App) -> Div {
    div()
        .h_px()
        .mx(px(-space::XS))
        .my(px(space::XS))
        .bg(surface::floating_border(cx))
}

/// A group's name: small, muted, over the rows it names; a child sits at its end.
pub fn menu_header(title: SharedString) -> Div {
    h_flex()
        .w_full()
        .min_h(px(size::MENU_ROW - space::XS))
        .px(px(space::MD))
        .pt(px(space::XS))
        .gap(px(space::MD))
        .justify_between()
        .child(
            Label::new(title)
                .size(LabelSize::XSmall)
                .weight(FontWeight::MEDIUM)
                .color(Color::Muted)
                .single_line(),
        )
}

type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;
type HoverHandler = Box<dyn Fn(&bool, &mut Window, &mut App) + 'static>;

/// One row of a menu or a picker (the command palette): [`size::MENU_ROW`] high and inset
/// [`space::MD`] unless set, rounded within the card's corner. A hovered row and the
/// keyboard-highlighted one take the highlight fill, a pressed one the pressed fill; a
/// disabled row neither. It reports its role, label, shortcut and state to assistive
/// technology as Zed's list item does.
#[derive(IntoElement)]
pub struct MenuRow {
    id: ElementId,
    height: f32,
    radius: Option<f32>,
    inset: f32,
    disabled: bool,
    selected: bool,
    role: Option<Role>,
    checked: Option<bool>,
    active_descendant: bool,
    label: Option<SharedString>,
    keyshortcuts: Option<SharedString>,
    on_click: Option<ClickHandler>,
    on_hover: Option<HoverHandler>,
    start: Option<AnyElement>,
    end: Option<AnyElement>,
    children: Vec<AnyElement>,
}

impl MenuRow {
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            height: size::MENU_ROW,
            radius: None,
            inset: space::MD,
            disabled: false,
            selected: false,
            role: None,
            checked: None,
            active_descendant: false,
            label: None,
            keyshortcuts: None,
            on_click: None,
            on_hover: None,
            start: None,
            end: None,
            children: Vec::new(),
        }
    }

    /// A taller row, for a picker's (the command palette's 32 px rows).
    pub fn height(mut self, height: f32) -> Self {
        self.height = height;
        self
    }

    /// The corner, for a row in a card other than a menu's: the card's radius less its inset.
    pub fn radius(mut self, radius: f32) -> Self {
        self.radius = Some(radius);
        self
    }

    /// The space between the row's fill and its content either side, for a row led by an
    /// icon tile: half the row's height less the tile's, so the tile sits as far in from the
    /// fill's side as from its top.
    pub fn inset(mut self, inset: f32) -> Self {
        self.inset = inset;
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn toggle_state(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    pub fn aria_role(mut self, role: Role) -> Self {
        self.role = Some(role);
        self
    }

    pub fn aria_checked(mut self, checked: bool) -> Self {
        self.checked = Some(checked);
        self
    }

    pub fn aria_active_descendant(mut self) -> Self {
        self.active_descendant = true;
        self
    }

    pub fn aria_label(mut self, label: impl Into<SharedString>) -> Self {
        self.label = Some(label.into());
        self
    }

    pub fn aria_keyshortcuts(mut self, keyshortcuts: impl Into<SharedString>) -> Self {
        self.keyshortcuts = Some(keyshortcuts.into());
        self
    }

    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Box::new(handler));
        self
    }

    pub fn on_hover(mut self, handler: impl Fn(&bool, &mut Window, &mut App) + 'static) -> Self {
        self.on_hover = Some(Box::new(handler));
        self
    }

    pub fn start_slot(mut self, start: impl IntoElement) -> Self {
        self.start = Some(start.into_any_element());
        self
    }

    pub fn end_slot(mut self, end: impl IntoElement) -> Self {
        self.end = Some(end.into_any_element());
        self
    }
}

impl ParentElement for MenuRow {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements)
    }
}

impl RenderOnce for MenuRow {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let interactive = !self.disabled;
        let highlight = surface::highlight(cx);
        let pressed = surface::pressed(cx);
        h_flex()
            .id(self.id)
            .relative()
            .w_full()
            .min_h(px(self.height))
            .px(px(self.inset))
            .gap(px(space::MD))
            .rounded(px(self.radius.unwrap_or_else(|| radii(cx).menu_item())))
            .when_some(self.role, |this, role| {
                this.role(role)
                    .aria_selected(self.selected)
                    .when(self.active_descendant, |this| this.aria_active_descendant())
                    .when_some(self.checked, |this, checked| {
                        this.aria_toggled(if checked {
                            gpui::Toggled::True
                        } else {
                            gpui::Toggled::False
                        })
                    })
            })
            .when_some(self.label, |this, label| this.aria_label(label))
            .when_some(self.keyshortcuts, |this, keyshortcuts| {
                this.aria_keyshortcuts(keyshortcuts)
            })
            .when(interactive, |this| {
                this.hover(move |style| style.bg(highlight))
                    .active(move |style| style.bg(pressed))
                    .when(self.selected, |this| this.bg(highlight))
            })
            .when_some(self.on_hover, |this, on_hover| this.on_hover(on_hover))
            .when_some(
                self.on_click.filter(|_| !self.disabled),
                |this, on_click| this.cursor_pointer().on_click(on_click),
            )
            .children(self.start)
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(space::MD))
                    .overflow_hidden()
                    .children(self.children),
            )
            .children(self.end)
    }
}

#[cfg(test)]
mod tests {
    use gpui::{TestAppContext, VisualTestContext};

    use super::*;

    fn focus_menu(menu: &Entity<ContextMenu>, cx: &mut VisualTestContext) {
        cx.update(|window, cx| window.blur(cx));
        cx.run_until_parked();
        cx.update(|window, cx| window.focus(&menu.focus_handle(cx), cx));
        cx.run_until_parked();
    }

    fn assert_focus_selection(cx: &mut TestAppContext, persistent: bool, a11y_enabled: bool) {
        if !a11y_enabled {
            cx.disable_accessibility();
        }
        cx.update(|cx| {
            studio_theme::init_test(cx);
        });

        let window = cx
            .update(|cx| {
                cx.open_window(Default::default(), |window, cx| {
                    let build_menu =
                        |menu: ContextMenu, _: &mut Window, _: &mut Context<ContextMenu>| {
                            menu.header("Options")
                                .entry("First", None, |_, _| {})
                                .separator()
                                .item(
                                    ContextMenuEntry::new("Checked")
                                        .toggleable(IconPosition::Start, true),
                                )
                        };
                    if persistent {
                        ContextMenu::build_persistent(window, cx, build_menu)
                    } else {
                        ContextMenu::build(window, cx, build_menu)
                    }
                })
            })
            .expect("open menu window");
        let menu = window.root(cx).expect("menu root");
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        let focus_events = Rc::new(Cell::new(0));
        cx.update(|window, cx| {
            assert_eq!(window.is_a11y_enabled(), a11y_enabled);
            assert!(!window.is_a11y_active());
            let focus_events = focus_events.clone();
            window
                .on_focus_in(&menu.focus_handle(cx), cx, move |_, _| {
                    focus_events.set(focus_events.get() + 1);
                })
                .detach();
            window.activate_window();
        });
        cx.run_until_parked();

        menu.read_with(&cx, |menu, _| {
            assert_eq!(menu.selected_index, None);
            assert_eq!(menu.suppress_focus_selection, !a11y_enabled);
        });
        focus_menu(&menu, &mut cx);
        assert_eq!(focus_events.get(), 1);
        assert_eq!(
            menu.read_with(&cx, |menu, _| menu.selected_index),
            a11y_enabled.then_some(3),
        );

        cx.dispatch_action(SelectNext);
        assert_eq!(
            menu.read_with(&cx, |menu, _| menu.selected_index),
            Some(1),
            "SelectNext right after opening should select the first selectable entry",
        );
        cx.dispatch_action(SelectNext);
        assert_eq!(menu.read_with(&cx, |menu, _| menu.selected_index), Some(3));
        cx.dispatch_action(SelectPrevious);
        assert_eq!(menu.read_with(&cx, |menu, _| menu.selected_index), Some(1));

        focus_menu(&menu, &mut cx);
        assert_eq!(focus_events.get(), 2);
        assert_eq!(menu.read_with(&cx, |menu, _| menu.selected_index), Some(1));

        menu.update(&mut cx, |menu, cx| {
            menu.clear_selected();
            if a11y_enabled {
                menu.suppress_focus_selection = true;
            }
            cx.notify();
        });
        focus_menu(&menu, &mut cx);
        assert_eq!(focus_events.get(), 3);
        menu.read_with(&cx, |menu, _| {
            assert_eq!(menu.selected_index, None);
            assert_eq!(menu.suppress_focus_selection, !a11y_enabled);
        });

        focus_menu(&menu, &mut cx);
        assert_eq!(focus_events.get(), 4);
        assert_eq!(
            menu.read_with(&cx, |menu, _| menu.selected_index),
            a11y_enabled.then_some(3),
        );
    }

    #[gpui::test]
    fn focus_selection_with_accessibility(cx: &mut TestAppContext) {
        assert_focus_selection(cx, false, true);
    }

    #[gpui::test]
    fn focus_selection_without_accessibility(cx: &mut TestAppContext) {
        assert_focus_selection(cx, false, false);
    }

    #[gpui::test]
    fn persistent_focus_selection_with_accessibility(cx: &mut TestAppContext) {
        assert_focus_selection(cx, true, true);
    }

    #[gpui::test]
    fn persistent_focus_selection_without_accessibility(cx: &mut TestAppContext) {
        assert_focus_selection(cx, true, false);
    }

    #[gpui::test]
    fn confirm_without_selection_dismisses(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let entry_handler_calls = Rc::new(Cell::new(0));
        let context_menu = cx.update(|window, cx| {
            ContextMenu::build(window, cx, {
                let entry_handler_calls = entry_handler_calls.clone();
                move |menu, _, _| {
                    menu.header("Header").entry("Entry", None, move |_, _| {
                        entry_handler_calls.set(entry_handler_calls.get() + 1);
                    })
                }
            })
        });

        let dismiss_events = Rc::new(Cell::new(0));
        let _subscription = cx.update(|_, cx| {
            let dismiss_events = dismiss_events.clone();
            cx.subscribe(&context_menu, move |_, _: &DismissEvent, _| {
                dismiss_events.set(dismiss_events.get() + 1);
            })
        });

        context_menu.update_in(cx, |context_menu, window, cx| {
            assert_eq!(None, context_menu.selected_index);
            context_menu.confirm(&menu::Confirm, window, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            dismiss_events.get(),
            1,
            "Confirming without a selection should dismiss the menu"
        );
        assert_eq!(
            entry_handler_calls.get(),
            0,
            "Confirming without a selection should not trigger any entry"
        );
    }

    #[gpui::test]
    fn can_navigate_back_over_headers(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let context_menu = cx.update(|window, cx| {
            ContextMenu::build(window, cx, |menu, _, _| {
                menu.header("First header")
                    .separator()
                    .entry("First entry", None, |_, _| {})
                    .separator()
                    .separator()
                    .entry("Last entry", None, |_, _| {})
                    .header("Last header")
            })
        });

        context_menu.update_in(cx, |context_menu, window, cx| {
            assert_eq!(
                None, context_menu.selected_index,
                "No selection is in the menu initially"
            );

            context_menu.select_first(&SelectFirst, window, cx);
            assert_eq!(
                Some(2),
                context_menu.selected_index,
                "Should select first selectable entry, skipping the header and the separator"
            );

            context_menu.select_next(&SelectNext, window, cx);
            assert_eq!(
                Some(5),
                context_menu.selected_index,
                "Should select next selectable entry, skipping 2 separators along the way"
            );

            context_menu.select_next(&SelectNext, window, cx);
            assert_eq!(
                Some(2),
                context_menu.selected_index,
                "Should wrap around to first selectable entry"
            );
        });

        context_menu.update_in(cx, |context_menu, window, cx| {
            assert_eq!(
                Some(2),
                context_menu.selected_index,
                "Should start from the first selectable entry"
            );

            context_menu.select_previous(&SelectPrevious, window, cx);
            assert_eq!(
                Some(5),
                context_menu.selected_index,
                "Should wrap around to previous selectable entry (last)"
            );

            context_menu.select_previous(&SelectPrevious, window, cx);
            assert_eq!(
                Some(2),
                context_menu.selected_index,
                "Should go back to previous selectable entry (first)"
            );
        });

        context_menu.update_in(cx, |context_menu, window, cx| {
            context_menu.select_first(&SelectFirst, window, cx);
            assert_eq!(
                Some(2),
                context_menu.selected_index,
                "Should start from the first selectable entry"
            );

            context_menu.select_previous(&SelectPrevious, window, cx);
            assert_eq!(
                Some(5),
                context_menu.selected_index,
                "Should wrap around to last selectable entry"
            );
            context_menu.select_next(&SelectNext, window, cx);
            assert_eq!(
                Some(2),
                context_menu.selected_index,
                "Should wrap around to first selectable entry"
            );
        });
    }
}
