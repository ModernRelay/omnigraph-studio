//! Studio's buttons, after Tower's (spec 002, "Buttons"): every text button is a pill and every
//! icon button a circle, in one of six variants — primary (solid ink), secondary (a faint ink
//! fill), outline (a solid item with a hairline and a contact shadow), ghost (nothing until
//! hovered), destructive (solid red) and glass (a floating pill). Zed's `ButtonLike` keeps its
//! corners at 4 px and its colours to itself, so Studio draws its own; it keeps Zed's builder
//! traits, so `.style(ButtonStyle::…)` picks the variant (`Variant::from`) and a button is a
//! `PopoverMenu` trigger as Zed's is.
//!
//! A disabled button fades to 64 %; a toggled one sits on the pressed fill. Clicks stop at the
//! button and a press leaves the focus where it was, as Zed's buttons do.

use gpui::{
    AnyView, App, BoxShadow, ClickEvent, CursorStyle, DefiniteLength, ElementId, FocusHandle, Hsla,
    MouseButton, Role, Stateful, Window,
};
use ui::prelude::*;
use ui::{ElevationIndex, KeyBinding, TintColor};

use crate::ds;

/// What a button looks like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    /// The one action a surface is for: solid ink, white type.
    Primary,
    /// A quiet filled control: a faint ink fill.
    Secondary,
    /// A solid item: the surface's own fill, a hairline and a contact shadow. Cancel, and most
    /// controls on glass.
    Outline,
    /// Nothing until hovered: toolbars, rows, inline actions.
    Ghost,
    /// An action that destroys: solid red, white type.
    Destructive,
    /// A control floating over content (a canvas's bar): a small glass pill.
    Glass,
}

impl From<ButtonStyle> for Variant {
    fn from(style: ButtonStyle) -> Self {
        match style {
            // Studio marks a surface's main action Filled (Insert, Run, Merge, Save).
            ButtonStyle::Filled => Variant::Primary,
            ButtonStyle::Outlined | ButtonStyle::OutlinedGhost | ButtonStyle::OutlinedCustom(_) => {
                Variant::Outline
            }
            ButtonStyle::Tinted(TintColor::Error) => Variant::Destructive,
            ButtonStyle::Tinted(_) => Variant::Primary,
            ButtonStyle::Subtle | ButtonStyle::Transparent => Variant::Ghost,
        }
    }
}

/// A button's height for each of Zed's sizes: the medium one is a control's.
pub fn height(size: ButtonSize) -> Option<f32> {
    match size {
        ButtonSize::Large => Some(ds::size::LARGE_BUTTON),
        ButtonSize::Medium => Some(ds::size::CONTROL),
        ButtonSize::Default => Some(ds::size::BAR_BUTTON),
        ButtonSize::Compact => Some(ds::size::ROW_BUTTON),
        ButtonSize::None => None,
    }
}

/// A text button's padding either side of its content.
fn inset(size: ButtonSize) -> f32 {
    match size {
        ButtonSize::Large => ds::space::LG,
        ButtonSize::Medium => ds::space::CONTROL_INSET,
        ButtonSize::Default => ds::space::MD,
        ButtonSize::Compact => ds::space::SM,
        ButtonSize::None => 0.,
    }
}

/// The paint of one variant, in one state.
struct Paint {
    fill: Hsla,
    hover: Hsla,
    active: Hsla,
    border: Hsla,
    shadows: Vec<BoxShadow>,
    /// The type and icons' colour.
    ink: Color,
}

fn paint(variant: Variant, selected: bool, cx: &App) -> Paint {
    let colors = cx.theme().colors();
    let ink = colors.text;
    let clear = gpui::transparent_black();
    let pressed = ink.opacity(0.07);
    match variant {
        Variant::Primary => Paint {
            fill: ink,
            hover: ink.opacity(0.9),
            active: ink.opacity(0.84),
            // Its ring in its own fill, as interfaces.dev's primary.
            border: ink,
            shadows: ds::elevation::filled(ink),
            ink: Color::Custom(colors.editor_background),
        },
        Variant::Destructive => {
            let red = cx.theme().status().error;
            Paint {
                fill: red,
                hover: red.opacity(0.9),
                active: red.opacity(0.84),
                // As the primary, in red.
                border: red,
                shadows: ds::elevation::filled(red),
                ink: Color::Custom(gpui::white()),
            }
        }
        // interfaces.dev's secondary button: its hairline and two soft drops
        // (`shadow-custom`), and a fill a shade off white under the pointer.
        Variant::Outline => Paint {
            fill: if selected {
                colors.editor_background.blend(pressed)
            } else {
                colors.editor_background
            },
            hover: ds::surface::control_hover(colors.editor_background, cx),
            active: colors.editor_background.blend(pressed),
            border: clear,
            shadows: ds::elevation::control(cx),
            ink: Color::Default,
        },
        Variant::Secondary => Paint {
            fill: if selected {
                pressed
            } else {
                ds::surface::highlight(cx)
            },
            hover: ink.opacity(0.065),
            active: ink.opacity(0.08),
            border: clear,
            shadows: Vec::new(),
            ink: Color::Default,
        },
        Variant::Ghost => Paint {
            fill: if selected { pressed } else { clear },
            hover: ds::surface::highlight(cx),
            active: pressed,
            border: clear,
            shadows: Vec::new(),
            ink: Color::Default,
        },
        Variant::Glass => Paint {
            fill: ds::surface::glass(colors.elevated_surface_background, 0.92, cx),
            hover: ds::surface::glass(colors.elevated_surface_background, 0.98, cx),
            active: ds::surface::glass(colors.elevated_surface_background, 0.98, cx).blend(pressed),
            border: ds::surface::outline(cx),
            shadows: ds::elevation::float(cx),
            ink: Color::Default,
        },
    }
}

/// A button's outline. A button with a label is a pill; an icon button, and a button set into
/// a bar, takes the radius scale's `sm` corner (spec 002, "Corners").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Pill,
    Rounded,
}

/// A labelled button's text: the body size at a control's height and larger (Tower's text-sm,
/// 13 px at the default 15 px interface size), the small size in the header strips and dense
/// rows.
fn label_size(size: ButtonSize) -> LabelSize {
    match size {
        ButtonSize::Large | ButtonSize::Medium => LabelSize::Default,
        _ => LabelSize::Small,
    }
}

/// A labelled button's icon: 16 px at a control's height, smaller with the button.
fn label_icon_size(size: ButtonSize) -> IconSize {
    match size {
        ButtonSize::Large | ButtonSize::Medium => IconSize::Medium,
        _ => IconSize::XSmall,
    }
}

/// An icon button's icon, which has no label beside it: 14 px in a control's square and a
/// strip's, 12 px in a row's, 16 px in the large square. The button's size decides it, so
/// every icon button of one size draws its icon alike.
fn icon_button_icon_size(size: ButtonSize) -> IconSize {
    match size {
        ButtonSize::Large => IconSize::Medium,
        ButtonSize::Medium | ButtonSize::Default => IconSize::Small,
        ButtonSize::Compact | ButtonSize::None => IconSize::XSmall,
    }
}

type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;
type TooltipBuilder = Box<dyn Fn(&mut Window, &mut App) -> AnyView + 'static>;

/// What every button has: its id, look, size, state and handlers.
struct Base {
    id: ElementId,
    variant: Variant,
    size: ButtonSize,
    shape: Shape,
    disabled: bool,
    selected: bool,
    width: Option<DefiniteLength>,
    on_click: Option<ClickHandler>,
    tooltip: Option<TooltipBuilder>,
    tab_index: Option<isize>,
    focus_handle: Option<FocusHandle>,
    cursor_style: CursorStyle,
    visible_on_hover: Option<SharedString>,
    aria_label: Option<SharedString>,
}

impl Base {
    fn new(id: ElementId, variant: Variant, size: ButtonSize, shape: Shape) -> Self {
        Self {
            id,
            variant,
            size,
            shape,
            disabled: false,
            selected: false,
            width: None,
            on_click: None,
            tooltip: None,
            tab_index: None,
            focus_handle: None,
            cursor_style: CursorStyle::PointingHand,
            visible_on_hover: None,
            aria_label: None,
        }
    }

    /// The pill (or circle) with its paint, states and handlers; the caller adds the content.
    /// `paint` is the variant's, which the caller read its ink from.
    fn frame(self, paint: Paint, cx: &App) -> Stateful<Div> {
        let focused_border = cx.theme().colors().border_focused;
        let disabled = self.disabled;
        div()
            .id(self.id)
            .role(Role::Button)
            .when_some(self.aria_label, |this, label| this.aria_label(label))
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .when_some(height(self.size), |this, h| this.h(px(h)))
            .when_some(self.width, |this, width| this.w(width))
            .map(|this| match self.shape {
                Shape::Pill => this.rounded_full(),
                Shape::Rounded => this.rounded(px(ds::radii(cx).sm)),
            })
            .border_1()
            .border_color(paint.border)
            .bg(paint.fill)
            .shadow(paint.shadows)
            .text_color(paint.ink.color(cx))
            .when_some(self.tab_index, |this, index| {
                this.tab_index(index).tab_stop(!disabled)
            })
            .when_some(self.focus_handle, |this, handle| this.track_focus(&handle))
            .when(disabled, |this| this.opacity(0.64).cursor_not_allowed())
            .when(!disabled, |this| {
                let (hover, active) = (paint.hover, paint.active);
                this.cursor(self.cursor_style)
                    .hover(move |style| style.bg(hover))
                    .active(move |style| style.bg(active))
                    .focus_visible(move |style| style.border_color(focused_border))
            })
            .when_some(self.visible_on_hover, |this, group| {
                this.visible_on_hover(group)
            })
            .when_some(self.on_click.filter(|_| !disabled), |this, on_click| {
                this.on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
                    .on_click(move |event, window, cx| {
                        cx.stop_propagation();
                        on_click(event, window, cx)
                    })
            })
            .when_some(self.tooltip, |this, tooltip| {
                this.tooltip(move |window, cx| tooltip(window, cx))
            })
    }
}

/// A text button: a pill holding an optional start icon, the label, a key binding and an
/// optional end icon. What `ds::button` makes.
#[derive(IntoElement)]
pub struct Button {
    base: Base,
    label: SharedString,
    label_color: Option<Color>,
    label_size: Option<LabelSize>,
    start_icon: Option<Icon>,
    end_icon: Option<Icon>,
    key_binding: Option<KeyBinding>,
    key_caps: Option<ds::KeyCaps>,
}

impl Button {
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Self {
        let label = label.into();
        let mut base = Base::new(id.into(), Variant::Ghost, ButtonSize::Medium, Shape::Pill);
        base.aria_label = Some(label.clone());
        Self {
            base,
            label,
            label_color: None,
            label_size: None,
            start_icon: None,
            end_icon: None,
            key_binding: None,
            key_caps: None,
        }
    }

    /// The look, directly; `.style(ButtonStyle::…)` picks it from Zed's styles.
    pub fn variant(mut self, variant: Variant) -> Self {
        self.base.variant = variant;
        self
    }

    /// The outline: a pill unless the button sits in a bar.
    pub fn shape(mut self, shape: Shape) -> Self {
        self.base.shape = shape;
        self
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

    /// The keys as key caps, one box per key, after the label: for a launcher whose keys are
    /// part of what it says (the Conversations panel's "Ask the agent ⌘⇧A").
    pub fn key_caps(mut self, key_caps: ds::KeyCaps) -> Self {
        self.key_caps = Some(key_caps);
        self
    }
}

impl Toggleable for Button {
    fn toggle_state(mut self, selected: bool) -> Self {
        self.base.selected = selected;
        self
    }
}

impl Disableable for Button {
    fn disabled(mut self, disabled: bool) -> Self {
        self.base.disabled = disabled;
        self
    }
}

impl Clickable for Button {
    fn on_click(mut self, handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self {
        self.base.on_click = Some(Box::new(handler));
        self
    }

    fn cursor_style(mut self, cursor_style: CursorStyle) -> Self {
        self.base.cursor_style = cursor_style;
        self
    }
}

impl FixedWidth for Button {
    fn width(mut self, width: impl Into<DefiniteLength>) -> Self {
        self.base.width = Some(width.into());
        self
    }

    fn full_width(mut self) -> Self {
        self.base.width = Some(relative(1.));
        self
    }
}

impl ButtonCommon for Button {
    fn id(&self) -> &ElementId {
        &self.base.id
    }

    fn style(mut self, style: ButtonStyle) -> Self {
        self.base.variant = style.into();
        self
    }

    fn size(mut self, size: ButtonSize) -> Self {
        self.base.size = size;
        self
    }

    fn tooltip(mut self, tooltip: impl Fn(&mut Window, &mut App) -> AnyView + 'static) -> Self {
        self.base.tooltip = Some(Box::new(tooltip));
        self
    }

    fn tab_index(mut self, tab_index: impl Into<isize>) -> Self {
        self.base.tab_index = Some(tab_index.into());
        self
    }

    fn layer(self, _elevation: ElevationIndex) -> Self {
        self
    }

    fn track_focus(mut self, focus_handle: &FocusHandle) -> Self {
        self.base.focus_handle = Some(focus_handle.clone());
        self
    }
}

impl VisibleOnHover for Button {
    fn visible_on_hover(mut self, group_name: impl Into<SharedString>) -> Self {
        self.base.visible_on_hover = Some(group_name.into());
        self
    }
}

impl RenderOnce for Button {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let size = self.base.size;
        let paint = paint(self.base.variant, self.base.selected, cx);
        let painted = paint.ink;
        let ink = self.label_color.unwrap_or(painted);
        // Ink on solid fills always; a caller's colour only on the quiet variants.
        let ink = match self.base.variant {
            Variant::Primary | Variant::Destructive => painted,
            _ => ink,
        };
        // Icons at the label's scale and, as Tower's, at 80 % of its ink.
        let icon_ink = Color::Custom(ink.color(cx).opacity(0.8));
        let icon = |icon: Icon| icon.size(label_icon_size(size)).color(icon_ink);
        // A trailing icon is a chevron or a mark, not a picture: a step smaller.
        let end_icon = |icon: Icon| icon.size(IconSize::XSmall).color(icon_ink);
        // A key on a solid fill takes the fill's ink, a little quieter; elsewhere its own.
        let key_ink = match self.base.variant {
            Variant::Primary | Variant::Destructive => {
                Some(Color::Custom(painted.color(cx).opacity(0.72)))
            }
            _ => None,
        };
        let pad = inset(size);
        // A full-width button with keys reads as a launcher: its label at the start, its keys
        // at the end. Without keys its content stays centred.
        let spread =
            self.base.width.is_some() && (self.key_binding.is_some() || self.key_caps.is_some());
        self.base.frame(paint, cx).child(
            h_flex()
                .when(spread, |content| content.w_full())
                .px(px(pad - 1.))
                .gap(px(match size {
                    ButtonSize::Large | ButtonSize::Medium => ds::space::SM,
                    _ => ds::space::XS,
                }))
                .children(self.start_icon.map(icon))
                .child(
                    h_flex()
                        .debug_selector(|| "button-label".into())
                        .when(spread, |label| label.flex_1())
                        .gap(px(ds::space::SM))
                        .justify_between()
                        .child(
                            Label::new(self.label)
                                .color(ink)
                                .weight(ds::text::weight::BODY)
                                .size(self.label_size.unwrap_or(label_size(size))),
                        )
                        .children(self.key_binding.map(|key| match key_ink {
                            Some(ink) => key.color(ink),
                            None => key,
                        }))
                        .children(self.key_caps),
                )
                .children(self.end_icon.map(end_icon)),
        )
    }
}

/// An icon button: one icon in a square of the radius scale's small corner. By default Zed's
/// 22 px size (`size::BAR_BUTTON`), which every strip, toolbar and floating bar holds with
/// room around its fill; `ds::icon_button` is the 28 px one, beside a field or a medium
/// button.
#[derive(IntoElement)]
pub struct IconButton {
    base: Base,
    icon: IconName,
    icon_size: Option<IconSize>,
    icon_color: Option<Color>,
    selected_icon: Option<IconName>,
    selected_icon_color: Option<Color>,
}

impl IconButton {
    pub fn new(id: impl Into<ElementId>, icon: IconName) -> Self {
        Self {
            base: Base::new(
                id.into(),
                Variant::Ghost,
                ButtonSize::Default,
                Shape::Rounded,
            ),
            icon,
            icon_size: None,
            icon_color: None,
            selected_icon: None,
            selected_icon_color: None,
        }
    }

    pub fn variant(mut self, variant: Variant) -> Self {
        self.base.variant = variant;
        self
    }

    /// The outline: the radius scale's corner unless asked to be round.
    pub fn shape(mut self, shape: Shape) -> Self {
        self.base.shape = shape;
        self
    }

    /// An icon size other than the one the button's size gives it, for the one role that has
    /// one (`ds::sheet_close_button`); no view sets it.
    pub(crate) fn icon_size(mut self, size: IconSize) -> Self {
        self.icon_size = Some(size);
        self
    }

    pub fn icon_color(mut self, color: Color) -> Self {
        self.icon_color = Some(color);
        self
    }

    /// The icon shown while toggled on.
    pub fn selected_icon(mut self, icon: impl Into<Option<IconName>>) -> Self {
        self.selected_icon = icon.into();
        self
    }

    pub fn selected_icon_color(mut self, color: impl Into<Option<Color>>) -> Self {
        self.selected_icon_color = color.into();
        self
    }

    /// The name a screen reader says; the tooltip's text is not read.
    pub fn aria_label(mut self, label: impl Into<SharedString>) -> Self {
        self.base.aria_label = Some(label.into());
        self
    }
}

impl Toggleable for IconButton {
    fn toggle_state(mut self, selected: bool) -> Self {
        self.base.selected = selected;
        self
    }
}

impl Disableable for IconButton {
    fn disabled(mut self, disabled: bool) -> Self {
        self.base.disabled = disabled;
        self
    }
}

impl Clickable for IconButton {
    fn on_click(mut self, handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self {
        self.base.on_click = Some(Box::new(handler));
        self
    }

    fn cursor_style(mut self, cursor_style: CursorStyle) -> Self {
        self.base.cursor_style = cursor_style;
        self
    }
}

impl ButtonCommon for IconButton {
    fn id(&self) -> &ElementId {
        &self.base.id
    }

    fn style(mut self, style: ButtonStyle) -> Self {
        self.base.variant = style.into();
        self
    }

    fn size(mut self, size: ButtonSize) -> Self {
        self.base.size = size;
        self
    }

    fn tooltip(mut self, tooltip: impl Fn(&mut Window, &mut App) -> AnyView + 'static) -> Self {
        self.base.tooltip = Some(Box::new(tooltip));
        self
    }

    fn tab_index(mut self, tab_index: impl Into<isize>) -> Self {
        self.base.tab_index = Some(tab_index.into());
        self
    }

    fn layer(self, _elevation: ElevationIndex) -> Self {
        self
    }

    fn track_focus(mut self, focus_handle: &FocusHandle) -> Self {
        self.base.focus_handle = Some(focus_handle.clone());
        self
    }
}

impl VisibleOnHover for IconButton {
    fn visible_on_hover(mut self, group_name: impl Into<SharedString>) -> Self {
        self.base.visible_on_hover = Some(group_name.into());
        self
    }
}

impl RenderOnce for IconButton {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let selected = self.base.selected;
        let paint = paint(self.base.variant, selected, cx);
        let painted = paint.ink;
        let color = match self.base.variant {
            Variant::Primary | Variant::Destructive => painted,
            _ if selected => self
                .selected_icon_color
                .or(self.icon_color)
                .unwrap_or(painted),
            _ => self.icon_color.unwrap_or(Color::Muted),
        };
        let icon = match (selected, self.selected_icon) {
            (true, Some(icon)) => icon,
            _ => self.icon,
        };
        let side = height(self.base.size);
        let icon_size = self
            .icon_size
            .unwrap_or_else(|| icon_button_icon_size(self.base.size));
        self.base
            .frame(paint, cx)
            .when_some(side, |this, side| this.w(px(side)))
            .child(Icon::new(icon).size(icon_size).color(color))
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
                // Each size by its role. Before `button`, whose label the test measures.
                .child(
                    div()
                        .debug_selector(|| "toolbar-icon-button".into())
                        .child(ds::toolbar_icon_button("t", ui::IconName::Plus)),
                )
                .child(
                    div()
                        .debug_selector(|| "row-button".into())
                        .child(ds::row_button("r", "Show all")),
                )
                .child(
                    div()
                        .debug_selector(|| "row-icon-button".into())
                        .child(ds::row_icon_button("ri", ui::IconName::Copy)),
                )
                .child(
                    div()
                        .debug_selector(|| "inline-icon-button".into())
                        .child(ds::inline_icon_button("ii", ui::IconName::Close)),
                )
                .child(
                    div()
                        .debug_selector(|| "icon-button".into())
                        .child(ds::icon_button("x", ui::IconName::Close)),
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
                .child(
                    div()
                        .debug_selector(|| "icon-tile".into())
                        .child(ds::icon_tile(ui::IconName::Table, cx)),
                )
                .child(
                    div()
                        .debug_selector(|| "segment".into())
                        .child(ds::segment("s", "All", true, cx)),
                )
                .child(
                    div()
                        .debug_selector(|| "header-cell".into())
                        .child(ds::header_cell(Some(ui::IconName::Hash), "count", false)),
                )
                .child(
                    div()
                        .debug_selector(|| "data-cell".into())
                        .child(ds::data_cell().child(ds::text::body("12"))),
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
                .child(
                    div().debug_selector(|| "text-button".into()).child(
                        ds::button("t", "Apply")
                            .on_click(cx.listener(|this, _, _, _| this.pressed += 10)),
                    ),
                )
        }
    }

    /// Pressing a pill's button, or a text button, leaves the focus in the field and the click
    /// with the button, as Zed's buttons do.
    #[gpui::test]
    fn a_pressed_button_keeps_the_focus_and_the_click(cx: &mut TestAppContext) {
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
        for selector in ["pill", "text-button"] {
            let bounds = cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("{selector} was drawn"));
            cx.simulate_click(bounds.center(), gpui::Modifiers::none());
            cx.run_until_parked();
        }
        probe.read_with(cx, |probe, _| {
            assert_eq!((probe.pressed, probe.outer_clicks), (11, 0));
        });
        cx.update(|window, _| assert!(field.is_focused(window), "the field kept the focus"));
    }

    /// The design system's controls draw at their tokens: a button as tall as a control with its
    /// content 10 px in, an icon button a circle, a field as tall as a button, a pill button
    /// round in a pill, a floating bar's buttons inside its border, and each button role at its
    /// size, an icon button's icon following that size.
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
        assert_eq!(f32::from(content.origin.x - button.origin.x), 10.);
        assert_eq!(
            f32::from(button.right() - content.right()),
            10.,
            "the inset is even"
        );
        let icon = bounds(cx, "icon-button");
        assert_eq!(f32::from(icon.size.height), ds::size::CONTROL);
        assert_eq!(icon.size.width, icon.size.height, "a circle");
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
        for (selector, side) in [
            ("toolbar-icon-button", ds::size::BAR_BUTTON),
            ("row-button", ds::size::ROW_BUTTON),
            ("row-icon-button", ds::size::ROW_BUTTON),
        ] {
            assert_eq!(
                f32::from(bounds(cx, selector).size.height),
                side,
                "{selector}"
            );
        }
        // A tile is a square; a segment a bar button's height, raised or not; a grid's cells
        // a header's and a row's.
        let tile = bounds(cx, "icon-tile");
        assert_eq!(f32::from(tile.size.height), ds::size::ICON_TILE);
        assert_eq!(tile.size.width, tile.size.height, "a square");
        for (selector, side) in [
            ("segment", ds::size::BAR_BUTTON),
            ("header-cell", ds::size::HEADER),
            ("data-cell", ds::size::ROW),
        ] {
            assert_eq!(
                f32::from(bounds(cx, selector).size.height),
                side,
                "{selector}"
            );
        }
        let row_icon = bounds(cx, "row-icon-button");
        assert_eq!(row_icon.size.width, row_icon.size.height, "a square");
        assert!(
            f32::from(bounds(cx, "inline-icon-button").size.height) < ds::size::ROW_BUTTON,
            "a tab's icon has no square beyond itself"
        );
        // `IconSize` has no `Debug`.
        use ui::{ButtonSize, IconSize};
        for (size, icon) in [
            (ButtonSize::Large, IconSize::Medium),
            (ButtonSize::Medium, IconSize::Small),
            (ButtonSize::Default, IconSize::Small),
            (ButtonSize::Compact, IconSize::XSmall),
            (ButtonSize::None, IconSize::XSmall),
        ] {
            assert!(super::icon_button_icon_size(size) == icon);
        }
    }

    /// Zed's styles pick Tower's variants: the quiet ones stay quiet, a tint is the action.
    #[test]
    fn zed_styles_pick_the_variants() {
        use super::Variant;
        use ui::{ButtonStyle, TintColor};
        assert_eq!(Variant::from(ButtonStyle::Subtle), Variant::Ghost);
        assert_eq!(Variant::from(ButtonStyle::Filled), Variant::Primary);
        assert_eq!(Variant::from(ButtonStyle::Outlined), Variant::Outline);
        assert_eq!(
            Variant::from(ButtonStyle::Tinted(TintColor::Accent)),
            Variant::Primary
        );
        assert_eq!(
            Variant::from(ButtonStyle::Tinted(TintColor::Error)),
            Variant::Destructive
        );
    }
}
