//! Tooltips under their control (spec 002, "Visual language"): GPUI draws a tooltip at the
//! pointer, so one over a short control in a toolbar lands on the rows beneath, and its place
//! follows wherever the pointer was. Studio's tooltips are anchored instead: the control
//! records its bounds as it is laid out ([`Anchor`]), and the tooltip draws under the
//! control's left edge, a gap below it, flipping above when the window ends first.
//!
//! Views take [`Tipped::tip`] with [`tooltip`], [`tooltip_for_action`] and
//! [`tooltip_for_action_in`] (`script/check-design-tokens.py` refuses `ui::Tooltip` and a bare
//! `.tooltip(` in a view); a tooltip built from state takes [`Tipped::tip_fn`] with the same
//! helpers' `*_view` forms.

use std::any::TypeId;
use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    Action, AnyView, App, Bounds, Context, Div, FocusHandle, Pixels, Point, Render, Size, Stateful,
    Window, canvas, point, px,
};
use ui::prelude::*;
use ui::{KeyBinding, Tooltip as ZedTooltip};

use crate::ds::{self, size, space};

/// What a tooltip says: its title, a key binding when it names an action, and a line of
/// detail under it.
#[derive(Clone)]
pub struct TipContent {
    title: SharedString,
    meta: Option<SharedString>,
    key: Option<Key>,
}

#[derive(Clone)]
enum Key {
    Action(Rc<dyn Action>),
    ActionIn(Rc<dyn Action>, FocusHandle),
}

/// A tooltip saying `title`.
pub fn tooltip(title: impl Into<SharedString>) -> TipContent {
    TipContent {
        title: title.into(),
        meta: None,
        key: None,
    }
}

/// A tooltip saying `title`, with `action`'s key as the window binds it.
pub fn tooltip_for_action(title: impl Into<SharedString>, action: &dyn Action) -> TipContent {
    TipContent {
        key: Some(Key::Action(Rc::from(action.boxed_clone()))),
        ..tooltip(title)
    }
}

/// A tooltip saying `title`, with `action`'s key as it is bound where `focus` has the keyboard.
pub fn tooltip_for_action_in(
    title: impl Into<SharedString>,
    action: &dyn Action,
    focus: &FocusHandle,
) -> TipContent {
    TipContent {
        key: Some(Key::ActionIn(Rc::from(action.boxed_clone()), focus.clone())),
        ..tooltip(title)
    }
}

/// [`tooltip`] as a view, for a tooltip built from state ([`Tipped::tip_fn`]).
pub fn tooltip_view(title: impl Into<SharedString>, cx: &mut App) -> AnyView {
    tooltip(title).into_view(cx)
}

/// [`tooltip_for_action`] as a view, for a tooltip built from state ([`Tipped::tip_fn`]).
pub fn tooltip_for_action_view(
    title: impl Into<SharedString>,
    action: &dyn Action,
    cx: &mut App,
) -> AnyView {
    tooltip_for_action(title, action).into_view(cx)
}

/// [`tooltip_for_action_in`] as a view, for a tooltip built from state.
pub fn tooltip_for_action_in_view(
    title: impl Into<SharedString>,
    action: &dyn Action,
    focus: &FocusHandle,
    cx: &mut App,
) -> AnyView {
    tooltip_for_action_in(title, action, focus).into_view(cx)
}

/// A tooltip with a line of detail, and an action's key when it names one, as a view.
pub fn tooltip_with_meta(
    title: impl Into<SharedString>,
    action: Option<&dyn Action>,
    meta: impl Into<SharedString>,
    cx: &mut App,
) -> AnyView {
    match action {
        Some(action) => tooltip_for_action(title, action),
        None => tooltip(title),
    }
    .meta(meta)
    .into_view(cx)
}

impl TipContent {
    /// A line of detail under the title.
    pub fn meta(mut self, meta: impl Into<SharedString>) -> Self {
        self.meta = Some(meta.into());
        self
    }

    /// The tooltip as a view at the pointer, for [`Tipped::tip_fn`], which shifts it to the
    /// control.
    pub fn into_view(self, cx: &mut App) -> AnyView {
        cx.new(|_| Tip {
            content: self,
            offset: Point::default(),
        })
        .into()
    }
}

/// The tooltip's own view: the content in Studio's floating surface, drawn `offset` from where
/// GPUI puts a tooltip (the pointer).
struct Tip {
    content: TipContent,
    offset: Point<Pixels>,
}

impl Render for Tip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let key = match &self.content.key {
            Some(Key::Action(action)) => Some(KeyBinding::for_action(action.as_ref(), cx)),
            Some(Key::ActionIn(action, focus)) => {
                Some(KeyBinding::for_action_in(action.as_ref(), focus, cx))
            }
            None => None,
        };
        shifted(
            self.offset,
            ds::menu_surface(v_flex(), cx)
                .max_w(px(size::TOOLTIP_WIDTH))
                .py(px(space::XS))
                .px(px(space::MD))
                .child(
                    h_flex()
                        .gap(px(space::XL))
                        .justify_between()
                        .child(ds::text::plain(self.content.title.clone()))
                        .children(key),
                )
                .when_some(self.content.meta.clone(), |tip, meta| {
                    tip.child(ds::text::meta(meta))
                }),
        )
    }
}

/// A tooltip's frame holding `content`, `offset` from where GPUI puts it (the pointer): GPUI
/// lays the view out as a root, where a margin moves nothing, so the frame is a child set off
/// its zero-sized root.
fn shifted(offset: Point<Pixels>, content: impl IntoElement) -> Div {
    div().relative().size_0().child(
        div()
            .absolute()
            .left(offset.x)
            .top(offset.y)
            .flex()
            .flex_none()
            .child(content),
    )
}

/// A tooltip view made elsewhere, drawn `offset` from the pointer: Zed's own carries its
/// pointer inset, pulled back so it sits where Studio's would.
struct Shifted {
    inner: AnyView,
    offset: Point<Pixels>,
}

impl Render for Shifted {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        shifted(self.offset, self.inner.clone())
    }
}

/// Where a control was last laid out, recorded by its [`Anchor::element`], for the tooltip
/// built when it is hovered.
#[derive(Clone, Default)]
pub struct Anchor(Rc<Cell<Option<Bounds<Pixels>>>>);

impl Anchor {
    pub fn new() -> Self {
        Self::default()
    }

    /// The recorder: an element the size of its parent (which is positioned, `relative`) that
    /// draws nothing and takes no room.
    pub fn element(&self) -> impl IntoElement {
        let cell = self.0.clone();
        canvas(move |bounds, _, _| cell.set(Some(bounds)), |_, _, _, _| {})
            .absolute()
            .inset_0()
    }

    /// How far from the pointer the tooltip draws: under the control's left edge, a gap below
    /// it, kept inside the window and flipped above when the window ends first. `None` before
    /// the control was laid out, when GPUI's place stands.
    fn offset(&self, window: &Window) -> Option<Point<Pixels>> {
        let bounds = self.0.get()?;
        Some(place(
            bounds,
            window.mouse_position(),
            window.viewport_size(),
        ))
    }

    /// The builder GPUI calls when the control is hovered: `content` at the anchor.
    pub fn builder(
        &self,
        content: TipContent,
    ) -> impl Fn(&mut Window, &mut App) -> AnyView + use<> {
        let anchor = self.clone();
        move |window, cx| {
            // Unanchored, it keeps clear of the pointer as Zed's does.
            let offset = anchor.offset(window).unwrap_or_else(|| {
                point(px(size::ZED_TOOLTIP_INSET.0), px(size::ZED_TOOLTIP_INSET.1))
            });
            let content = content.clone();
            cx.new(|_| Tip { content, offset }).into()
        }
    }

    /// The builder for a tooltip made by `build` (one that depends on state): its view, shifted
    /// to the anchor.
    pub fn builder_fn<F: Fn(&mut Window, &mut App) -> AnyView + 'static>(
        &self,
        build: F,
    ) -> impl Fn(&mut Window, &mut App) -> AnyView + use<F> {
        let anchor = self.clone();
        move |window, cx| {
            let inner = build(window, cx);
            let Some(offset) = anchor.offset(window) else {
                return inner;
            };
            let pull = if inner.entity_type() == TypeId::of::<ZedTooltip>() {
                point(px(size::ZED_TOOLTIP_INSET.0), px(size::ZED_TOOLTIP_INSET.1))
            } else {
                Point::default()
            };
            cx.new(|_| Shifted {
                inner,
                offset: offset - pull,
            })
            .into()
        }
    }
}

/// How far from the pointer a tooltip draws for a control at `bounds`: under its left edge,
/// a gap below it, kept inside a window of `viewport`, and above the control when the window
/// ends first. GPUI puts a tooltip one pixel past the pointer.
fn place(bounds: Bounds<Pixels>, mouse: Point<Pixels>, viewport: Size<Pixels>) -> Point<Pixels> {
    let gap = px(space::SM);
    let x = bounds
        .left()
        .min(viewport.width - px(size::TOOLTIP_WIDTH) - gap)
        .max(gap);
    let below = bounds.bottom() + gap;
    let y = if below + px(size::TOOLTIP_HEIGHT) > viewport.height {
        bounds.top() - gap - px(size::TOOLTIP_HEIGHT)
    } else {
        below
    };
    let placed = mouse + point(px(1.), px(1.));
    point(x - placed.x, y - placed.y)
}

/// A control that takes an anchored tooltip.
pub trait Tipped: Sized {
    /// A tooltip saying `content`, under the control.
    fn tip(self, content: TipContent) -> Self;

    /// A tooltip built when the control is hovered, from state, under the control.
    fn tip_fn(self, build: impl Fn(&mut Window, &mut App) -> AnyView + 'static) -> Self;
}

impl Tipped for Stateful<Div> {
    fn tip(self, content: TipContent) -> Self {
        let anchor = Anchor::new();
        // The recorder is placed against its host, so the host is positioned.
        self.relative()
            .tooltip(anchor.builder(content))
            .child(anchor.element())
    }

    fn tip_fn(self, build: impl Fn(&mut Window, &mut App) -> AnyView + 'static) -> Self {
        let anchor = Anchor::new();
        self.relative()
            .tooltip(anchor.builder_fn(build))
            .child(anchor.element())
    }
}

/// Zed's `ButtonLike`, which the graph switcher is built on: it cannot be positioned, so it
/// holds no recorder, and its tooltip keeps the pointer's place in Studio's look.
impl Tipped for ui::ButtonLike {
    fn tip(self, content: TipContent) -> Self {
        self.tooltip(Anchor::new().builder(content))
    }

    fn tip_fn(self, build: impl Fn(&mut Window, &mut App) -> AnyView + 'static) -> Self {
        self.tooltip(Anchor::new().builder_fn(build))
    }
}

#[cfg(test)]
mod tests {
    use gpui::{Render, TestAppContext, size};

    use super::*;

    struct Host {
        anchor: Anchor,
    }

    impl Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().p(px(40.)).child(
                div()
                    .id("control")
                    .relative()
                    .w(px(60.))
                    .h(px(22.))
                    .tooltip(self.anchor.builder(tooltip("t")))
                    .child(self.anchor.element()),
            )
        }
    }

    /// The recorder takes its control's bounds as the control is laid out.
    #[gpui::test]
    async fn the_recorder_takes_its_controls_bounds(cx: &mut TestAppContext) {
        cx.update(studio_theme::init_test);
        let anchor = Anchor::new();
        let (_, cx) = cx.add_window_view(|_, _| Host {
            anchor: anchor.clone(),
        });
        cx.run_until_parked();
        assert_eq!(
            anchor.0.get(),
            Some(Bounds::new(point(px(40.), px(40.)), size(px(60.), px(22.))))
        );
    }

    /// A tooltip draws under its control's left edge, a gap below it, wherever the pointer is
    /// in the control; at the window's right it stays inside; at the bottom it flips above.
    #[test]
    fn a_tooltip_sits_under_its_control_and_flips_at_the_bottom() {
        let viewport = size(px(1000.), px(600.));
        let control = Bounds::new(point(px(200.), px(100.)), size(px(60.), px(22.)));
        let gap = px(space::SM);
        for mouse in [point(px(210.), px(105.)), point(px(255.), px(120.))] {
            let offset = place(control, mouse, viewport);
            // Where GPUI draws it, one pixel past the pointer, plus the offset.
            let drawn = mouse + point(px(1.), px(1.)) + offset;
            assert_eq!(drawn, point(px(200.), px(122.) + gap), "{mouse:?}");
        }
        let at_right = Bounds::new(point(px(980.), px(100.)), size(px(20.), px(22.)));
        let drawn = point(px(985.), px(110.))
            + point(px(1.), px(1.))
            + place(at_right, point(px(985.), px(110.)), viewport);
        assert_eq!(drawn.x, px(1000.) - px(size::TOOLTIP_WIDTH) - gap);
        let at_bottom = Bounds::new(point(px(200.), px(580.)), size(px(60.), px(20.)));
        let drawn = point(px(210.), px(590.))
            + point(px(1.), px(1.))
            + place(at_bottom, point(px(210.), px(590.)), viewport);
        assert_eq!(drawn.y, px(580.) - gap - px(size::TOOLTIP_HEIGHT));
    }
}
