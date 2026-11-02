//! Tooltips under their control (spec 002, "Visual language"): GPUI draws a tooltip at the
//! pointer, so one over a short control in a toolbar lands on the rows beneath, and its place
//! follows wherever the pointer was. Studio's tooltips are anchored instead: the control
//! records its bounds as it is laid out ([`Anchor`]), and the tooltip hangs under the
//! control's left edge, a gap below it, or over the control when the window ends first, and
//! is measured where it draws so it stays inside the window.
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
    Window, anchored, canvas, point, px,
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
    /// A row of detail under a rule, built when the tooltip shows (a commit's counts and
    /// where it opens).
    detail: Option<Rc<Detail>>,
}

/// What builds a tooltip's detail row when it shows.
type Detail = dyn Fn(&mut App) -> AnyElement;

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
        detail: None,
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

    /// A row of detail under a rule, built when the tooltip shows.
    pub fn detail(mut self, build: impl Fn(&mut App) -> AnyElement + 'static) -> Self {
        self.detail = Some(Rc::new(build));
        self
    }

    /// How many lines it takes: the title, the line under it, and the detail row.
    fn lines(&self) -> usize {
        1 + usize::from(self.meta.is_some()) + 2 * usize::from(self.detail.is_some())
    }

    /// The tooltip as a view at the pointer, for [`Tipped::tip_fn`], which hangs it from the
    /// control.
    pub fn into_view(self, cx: &mut App) -> AnyView {
        cx.new(|_| Tip {
            content: self,
            at: None,
        })
        .into()
    }
}

/// Where a tooltip hangs from: a corner of it at a point of the window.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Placement {
    origin: Point<Pixels>,
    corner: gpui::Anchor,
}

/// The tooltip's own view: the content in Studio's floating surface, hung at `at`, or off the
/// pointer where GPUI puts a tooltip when it has no place.
struct Tip {
    content: TipContent,
    at: Option<Placement>,
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
        // A tooltip with a detail row is a hover card: roomier, its detail a grey footer
        // across the card under a hairline, as a card's ([`ds::card_footer`]).
        let card = self.content.detail.is_some();
        let (pad_x, pad_y) = if card {
            (space::LG, space::MD)
        } else {
            (space::MD, space::XS)
        };
        let menu = ds::radii(cx).menu();
        placed(
            self.at,
            Point::default(),
            ds::menu_surface(v_flex(), cx)
                .max_w(px(size::TOOLTIP_WIDTH))
                // A hover card takes the tooltip's whole width, so its footer's counts and
                // "Open" sit at its two ends.
                .when(card, |tip| tip.w(px(size::TOOLTIP_WIDTH)))
                .child(
                    v_flex()
                        .py(px(pad_y))
                        .px(px(pad_x))
                        .gap(px(if card { space::XS } else { 0. }))
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
                .when_some(self.content.detail.clone(), |tip, detail| {
                    tip.child(
                        div()
                            .rounded_b(px(ds::radius::flush(menu)))
                            .px(px(pad_x))
                            .py(px(space::SM))
                            .border_t_1()
                            .border_color(ds::surface::floating_border(cx))
                            .bg(ds::surface::footer(cx))
                            .child(detail(cx)),
                    )
                }),
        )
    }
}

/// A tooltip's frame holding `content`, hung at `at` in the window by GPUI's `anchored`, which
/// measures the frame and keeps it inside the window; `pull` is what the content sets itself
/// off by (Zed's own tooltip view), taken back. GPUI lays the tooltip view out as a root, where
/// a margin moves nothing, so the frame hangs off a zero-sized root. With no place (the control
/// not laid out yet, or one that cannot be positioned) it sits off the pointer, as Zed's does.
fn placed(at: Option<Placement>, pull: Point<Pixels>, content: impl IntoElement) -> Div {
    let root = div().relative().size_0();
    match at {
        Some(Placement { origin, corner }) => root.child(
            anchored()
                .position(origin - pull)
                .anchor(corner)
                .snap_to_window_with_margin(px(space::SM))
                .child(div().flex().flex_none().child(content)),
        ),
        None => {
            let inset = point(px(size::ZED_TOOLTIP_INSET.0), px(size::ZED_TOOLTIP_INSET.1)) - pull;
            root.child(
                div()
                    .absolute()
                    .left(inset.x)
                    .top(inset.y)
                    .flex()
                    .flex_none()
                    .child(content),
            )
        }
    }
}

/// A tooltip view made elsewhere, hung at `at`: Zed's own carries its pointer inset, pulled
/// back so it sits where Studio's would.
struct Shifted {
    inner: AnyView,
    at: Option<Placement>,
    pull: Point<Pixels>,
}

impl Render for Shifted {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        placed(self.at, self.pull, self.inner.clone())
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

    /// Where the tooltip hangs: under the control's left edge, a gap below it, or over the
    /// control when `lines` of it would not fit under. `None` before the control was laid out,
    /// when GPUI's place stands.
    fn placement(&self, lines: usize, window: &Window) -> Option<Placement> {
        let bounds = self.0.get()?;
        Some(place(bounds, lines, window.viewport_size()))
    }

    /// The builder GPUI calls when the control is hovered: `content` at the anchor.
    pub fn builder(
        &self,
        content: TipContent,
    ) -> impl Fn(&mut Window, &mut App) -> AnyView + use<> {
        let anchor = self.clone();
        move |window, cx| {
            let at = anchor.placement(content.lines(), window);
            let content = content.clone();
            cx.new(|_| Tip { content, at }).into()
        }
    }

    /// The builder for a tooltip made by `build` (one that depends on state): its view, hung
    /// from the anchor.
    pub fn builder_fn<F: Fn(&mut Window, &mut App) -> AnyView + 'static>(
        &self,
        build: F,
    ) -> impl Fn(&mut Window, &mut App) -> AnyView + use<F> {
        let anchor = self.clone();
        move |window, cx| {
            let inner = build(window, cx);
            let Some(at) = anchor.placement(1, window) else {
                return inner;
            };
            let pull = if inner.entity_type() == TypeId::of::<ZedTooltip>() {
                point(px(size::ZED_TOOLTIP_INSET.0), px(size::ZED_TOOLTIP_INSET.1))
            } else {
                Point::default()
            };
            cx.new(|_| Shifted {
                inner,
                at: Some(at),
                pull,
            })
            .into()
        }
    }
}

/// Where a tooltip of `lines` hangs for a control at `bounds` in a window of `viewport`: its
/// top-left corner a gap under the control's left edge, or its bottom-left corner a gap over
/// the control when that many lines would run past the window's bottom. Its width is measured
/// where it draws, and kept inside the window there.
fn place(bounds: Bounds<Pixels>, lines: usize, viewport: Size<Pixels>) -> Placement {
    let gap = px(space::SM);
    let below = bounds.bottom() + gap;
    let height = px(size::TOOLTIP_HEIGHT * lines.max(1) as f32);
    if below + height > viewport.height {
        Placement {
            origin: point(bounds.left(), bounds.top() - gap),
            corner: gpui::Anchor::BottomLeft,
        }
    } else {
        Placement {
            origin: point(bounds.left(), below),
            corner: gpui::Anchor::TopLeft,
        }
    }
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
        positioned(self)
            .tooltip(anchor.builder(content))
            .child(anchor.element())
    }

    fn tip_fn(self, build: impl Fn(&mut Window, &mut App) -> AnyView + 'static) -> Self {
        let anchor = Anchor::new();
        positioned(self)
            .tooltip(anchor.builder_fn(build))
            .child(anchor.element())
    }
}

/// `host`, positioned for the recorder, which is placed against it: made relative unless it
/// is placed already. Made relative over `absolute()`, a control placed in its parent (the
/// table's select-all box) fell back into the parent's flow.
fn positioned(mut host: Stateful<Div>) -> Stateful<Div> {
    if host.style().position.is_none() {
        host = host.relative();
    }
    host
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

    /// A control placed already keeps its place when it takes a tooltip; one that is not is
    /// made relative, for the recorder.
    #[test]
    fn a_tooltip_keeps_its_controls_place() {
        let mut placed = div().id("placed").absolute().tip(tooltip("t"));
        assert_eq!(placed.style().position, Some(gpui::Position::Absolute));
        let mut flowing = div().id("flowing").tip(tooltip("t"));
        assert_eq!(flowing.style().position, Some(gpui::Position::Relative));
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

    /// A tooltip hangs under its control's left edge, a gap below it; at the window's bottom it
    /// hangs over the control instead, and a taller tooltip flips sooner.
    #[test]
    fn a_tooltip_sits_under_its_control_and_flips_at_the_bottom() {
        let viewport = size(px(1000.), px(600.));
        let gap = px(space::SM);
        let control = Bounds::new(point(px(200.), px(100.)), size(px(60.), px(22.)));
        assert_eq!(
            place(control, 1, viewport),
            Placement {
                origin: point(px(200.), px(122.) + gap),
                corner: gpui::Anchor::TopLeft,
            }
        );
        let at_bottom = Bounds::new(point(px(200.), px(580.)), size(px(60.), px(20.)));
        assert_eq!(
            place(at_bottom, 1, viewport),
            Placement {
                origin: point(px(200.), px(580.) - gap),
                corner: gpui::Anchor::BottomLeft,
            }
        );
        // Under a control 50 px from the bottom one line fits; two do not.
        let near_bottom = Bounds::new(point(px(200.), px(530.)), size(px(60.), px(20.)));
        assert_eq!(
            place(near_bottom, 1, viewport).corner,
            gpui::Anchor::TopLeft
        );
        assert_eq!(
            place(near_bottom, 2, viewport).corner,
            gpui::Anchor::BottomLeft
        );
    }
}
