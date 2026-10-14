//! Studio's text button, what `ds::button` makes: Zed's `Button` with 6 px either side of its
//! content where Zed's medium button has 8. Zed fixes a button's padding by its size and keeps
//! its colours to itself, so this draws Zed's `ButtonLike` (colours, hover, press, focus,
//! toggled and disabled states) at a medium button's height around its own row: the start
//! icon, the label, a key binding and the end icon, coloured as Zed's `Button` colours them.

use gpui::{AnyView, App, ClickEvent, CursorStyle, DefiniteLength, ElementId, FocusHandle, Window};
use ui::prelude::*;
use ui::{ButtonLike, ElevationIndex, KeyBinding, rems_from_px};

#[derive(IntoElement)]
pub struct Button {
    base: ButtonLike,
    size: ButtonSize,
    label: SharedString,
    label_color: Option<Color>,
    label_size: Option<LabelSize>,
    start_icon: Option<Icon>,
    end_icon: Option<Icon>,
    key_binding: Option<KeyBinding>,
    disabled: bool,
    selected: bool,
}

impl Button {
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Self {
        let label = label.into();
        Self {
            base: ButtonLike::new(id).aria_label(label.clone()),
            size: ButtonSize::Medium,
            label,
            label_color: None,
            label_size: None,
            start_icon: None,
            end_icon: None,
            key_binding: None,
            disabled: false,
            selected: false,
        }
    }

    pub fn color(mut self, color: impl Into<Option<Color>>) -> Self {
        self.label_color = color.into();
        self
    }

    pub fn label_size(mut self, size: impl Into<Option<LabelSize>>) -> Self {
        self.label_size = size.into();
        self
    }

    pub fn start_icon(mut self, icon: impl Into<Option<Icon>>) -> Self {
        self.start_icon = icon.into();
        self
    }

    pub fn end_icon(mut self, icon: impl Into<Option<Icon>>) -> Self {
        self.end_icon = icon.into();
        self
    }

    /// The keys that do what the button does, after its label.
    pub fn key_binding(mut self, key_binding: impl Into<Option<KeyBinding>>) -> Self {
        self.key_binding = key_binding.into();
        self
    }
}

impl Toggleable for Button {
    fn toggle_state(mut self, selected: bool) -> Self {
        self.selected = selected;
        self.base = self.base.toggle_state(selected);
        self
    }
}

impl Disableable for Button {
    fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self.base = self.base.disabled(disabled);
        self
    }
}

impl Clickable for Button {
    fn on_click(mut self, handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self {
        self.base = self.base.on_click(handler);
        self
    }

    fn cursor_style(mut self, cursor_style: CursorStyle) -> Self {
        self.base = self.base.cursor_style(cursor_style);
        self
    }
}

impl FixedWidth for Button {
    fn width(mut self, width: impl Into<DefiniteLength>) -> Self {
        self.base = self.base.width(width);
        self
    }

    fn full_width(mut self) -> Self {
        self.base = self.base.full_width();
        self
    }
}

impl ButtonCommon for Button {
    fn id(&self) -> &ElementId {
        self.base.id()
    }

    fn style(mut self, style: ButtonStyle) -> Self {
        self.base = self.base.style(style);
        self
    }

    fn size(mut self, size: ButtonSize) -> Self {
        self.size = size;
        self
    }

    fn tooltip(mut self, tooltip: impl Fn(&mut Window, &mut App) -> AnyView + 'static) -> Self {
        self.base = self.base.tooltip(tooltip);
        self
    }

    fn tab_index(mut self, tab_index: impl Into<isize>) -> Self {
        self.base = self.base.tab_index(tab_index);
        self
    }

    fn layer(mut self, elevation: ElevationIndex) -> Self {
        self.base = self.base.layer(elevation);
        self
    }

    fn track_focus(mut self, focus_handle: &FocusHandle) -> Self {
        self.base = self.base.track_focus(focus_handle);
        self
    }
}

impl RenderOnce for Button {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        // A medium or large button: Zed's small padding (4 px) at its own height, and 2 px
        // more inside, so 6. Smaller sizes are Zed's as they are.
        let (base, inset) = match self.size {
            ButtonSize::Large => (
                self.base
                    .size(ButtonSize::Default)
                    .height(rems_from_px(32_f32).into()),
                true,
            ),
            ButtonSize::Medium => (
                self.base
                    .size(ButtonSize::Default)
                    .height(rems_from_px(28_f32).into()),
                true,
            ),
            size => (self.base.size(size), false),
        };
        let label_color = if self.disabled {
            Color::Disabled
        } else if self.selected {
            Color::Selected
        } else {
            self.label_color.unwrap_or_default()
        };
        let icon = |icon: Icon| {
            if self.disabled {
                icon.color(Color::Disabled)
            } else {
                icon
            }
        };
        base.child(
            h_flex()
                .when(inset, |row| row.px(DynamicSpacing::Base02.rems(cx)))
                .gap(DynamicSpacing::Base04.rems(cx))
                .children(self.start_icon.map(icon))
                .child(
                    h_flex()
                        .debug_selector(|| "button-label".into())
                        .gap(DynamicSpacing::Base06.rems(cx))
                        .justify_between()
                        .child(
                            Label::new(self.label)
                                .color(label_color)
                                .size(self.label_size.unwrap_or_default()),
                        )
                        .children(self.key_binding),
                )
                .children(self.end_icon.map(icon)),
        )
    }
}

#[cfg(test)]
mod tests {
    use gpui::{TestAppContext, VisualTestContext};

    use crate::ds;

    struct Probe;

    impl gpui::Render for Probe {
        fn render(
            &mut self,
            _window: &mut gpui::Window,
            cx: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            use ui::prelude::*;
            v_flex()
                .items_start()
                // First: its button's label shares the selector `button-label`, and the last
                // drawn holds it.
                .child(
                    ds::float_bar("bar", cx)
                        .debug_selector(|| "float-bar".into())
                        .child(
                            div()
                                .debug_selector(|| "bar-button".into())
                                .child(ds::bar_button("fit", "Fit")),
                        )
                        .child(
                            div()
                                .debug_selector(|| "bar-icon-button".into())
                                .child(ds::bar_icon_button("zoom", ui::IconName::Plus)),
                        ),
                )
                .child(
                    div()
                        .debug_selector(|| "button".into())
                        .child(ds::button("b", "Views")),
                )
                .child(
                    div()
                        .debug_selector(|| "field".into())
                        .child(ds::field_frame(false, cx).w(px(200.))),
                )
                .child(
                    div()
                        .debug_selector(|| "pill-button".into())
                        .child(ds::pill_button("p", ui::IconName::Close, Color::Muted, cx)),
                )
        }
    }

    struct PillProbe {
        field: gpui::FocusHandle,
        outer: gpui::FocusHandle,
        pressed: usize,
        outer_clicks: usize,
    }

    impl gpui::Render for PillProbe {
        fn render(
            &mut self,
            _window: &mut gpui::Window,
            cx: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            use ui::prelude::*;
            // A focusable surface around a field and a pill button, as a table around its
            // filter pill.
            div()
                .id("outer")
                .size_full()
                .track_focus(&self.outer)
                .on_click(cx.listener(|this, _, _, _| this.outer_clicks += 1))
                .child(div().id("field").size_8().track_focus(&self.field))
                .child(
                    ds::pill_button("p", ui::IconName::Check, Color::Accent, cx)
                        .debug_selector(|| "pill".into())
                        .on_click(cx.listener(|this, _, _, _| this.pressed += 1)),
                )
        }
    }

    /// Pressing a pill's button leaves the focus in the pill's field and the click with the
    /// button, as Zed's buttons do.
    #[gpui::test]
    fn a_pill_button_keeps_the_focus_and_the_click(cx: &mut TestAppContext) {
        cx.update(studio_theme::init_test);
        let (probe, cx): (_, &mut VisualTestContext) = cx.add_window_view(|_, cx| PillProbe {
            field: cx.focus_handle(),
            outer: cx.focus_handle(),
            pressed: 0,
            outer_clicks: 0,
        });
        let field = probe.read_with(cx, |probe, _| probe.field.clone());
        cx.update(|window, cx| window.focus(&field, cx));
        cx.run_until_parked();
        let pill = cx.debug_bounds("pill").expect("the pill button was drawn");
        cx.simulate_click(pill.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        probe.read_with(cx, |probe, _| {
            assert_eq!((probe.pressed, probe.outer_clicks), (1, 0));
        });
        cx.update(|window, _| assert!(field.is_focused(window), "the field kept the focus"));
    }

    /// The design system's controls draw at their tokens: a button 28 px tall with its content
    /// 6 px in, a field as tall as a button, a pill button round in a pill, and a floating
    /// bar's buttons inside its border.
    #[gpui::test]
    fn controls_draw_at_their_sizes(cx: &mut TestAppContext) {
        cx.update(studio_theme::init_test);
        let (_, cx): (_, &mut VisualTestContext) = cx.add_window_view(|_, _| Probe);
        cx.run_until_parked();
        let bounds = |cx: &mut VisualTestContext, selector| {
            cx.debug_bounds(selector)
                .unwrap_or_else(|| panic!("{selector} was not drawn"))
        };
        let button = bounds(cx, "button");
        // A label alone: what the inset is measured to.
        let content = bounds(cx, "button-label");
        assert_eq!(f32::from(button.size.height), ds::size::CONTROL);
        assert_eq!(f32::from(content.origin.x - button.origin.x), 6.);
        assert_eq!(
            f32::from(button.right() - content.right()),
            6.,
            "the inset is even"
        );
        assert_eq!(
            f32::from(bounds(cx, "field").size.height),
            ds::size::CONTROL
        );
        let pill = bounds(cx, "pill-button");
        assert_eq!(
            f32::from(pill.size.height),
            ds::size::PILL - 2. * ds::PILL_BUTTON_GAP
        );
        let bar = bounds(cx, "float-bar");
        assert_eq!(f32::from(bar.size.height), ds::size::CONTROL);
        for selector in ["bar-button", "bar-icon-button"] {
            let button = bounds(cx, selector);
            assert_eq!(
                f32::from(button.size.height),
                ds::size::BAR_BUTTON,
                "{selector}"
            );
            // Inside the bar's 1 px border, the same distance from its top and bottom.
            assert_eq!(
                f32::from(button.top() - bar.top()),
                f32::from(bar.bottom() - button.bottom()),
                "{selector}"
            );
            assert!(button.top() >= bar.top() + gpui::px(1.), "{selector}");
        }
    }
}
