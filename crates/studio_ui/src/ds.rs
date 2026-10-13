//! Studio's design system (spec 002, "Visual language"): tokens, then components built only
//! from tokens. Surfaces compose these; they do not pick their own paddings, radii, shadows or
//! colours. The gallery tab (`studio::OpenDesignSystem`) renders every piece in both window
//! styles, and changes are judged there first.
//!
//! Sources: Delta's window (measured from the mock on delta.dev) and Zed's `ui` components;
//! layout and polish rules: group with space, not lines; concentric radii; shadows for
//! elevation.

use gpui::{
    AnyElement, App, Background, BoxShadow, ClickEvent, Div, ElementId, Hsla, SharedString,
    Stateful, Window, point,
};
use studio_settings::{StudioSettings, WindowStyle};
use ui::prelude::*;
use ui::{Tooltip, rems_from_px};

// ---- tokens ----

/// The spacing scale, in pixels, and the roles it is used for. Every gap and padding in Studio
/// is one of these.
pub mod space {
    pub const XXS: f32 = 2.;
    pub const XS: f32 = 4.;
    pub const SM: f32 = 6.;
    pub const MD: f32 = 8.;
    pub const LG: f32 = 12.;
    pub const XL: f32 = 16.;
    pub const XXL: f32 = 24.;
    pub const XXXL: f32 = 32.;

    /// Between an icon and its label.
    pub const INLINE: f32 = SM;
    /// Between adjacent controls (better-layout: 12px between bordered controls).
    pub const CONTROLS: f32 = LG;
    /// Inside a group of rows.
    pub const WITHIN: f32 = XXS;
    /// Between groups: at least twice the space within them.
    pub const BETWEEN: f32 = XXL;
    /// Horizontal inset of rows, toolbars and strips from their column's edge.
    pub const EDGE: f32 = XL;
    /// Padding of a document page (settings, storage, empty states).
    pub const PAGE: f32 = XXL;
}

/// Corner radii. Nested surfaces stay concentric: inner = outer - padding (better-ui).
pub mod radius {
    /// Key caps, chips, code blocks.
    pub const XS: f32 = 4.;
    /// Rows, buttons, fields.
    pub const SM: f32 = 6.;
    /// Cards.
    pub const MD: f32 = 10.;
    /// Glass sheets: a card inset by `space::XS` inside one uses `MD`.
    pub const LG: f32 = 14.;
}

/// Fixed sizes.
pub mod size {
    /// Every column's header and footer strip, so their rules line up across the window.
    pub const STRIP: f32 = 40.;
    /// A surface's toolbar.
    pub const TOOLBAR: f32 = 48.;
    /// A data row and a data header: dense enough to scan a page at a glance, with room for a
    /// chip (an enum value, a relationship) inside a row.
    pub const ROW: f32 = 28.;
    pub const HEADER: f32 = 30.;
    /// A sidebar row.
    pub const SIDEBAR_ROW: f32 = 26.;
    /// A thread's or feed's gutter, where each entry's icon sits.
    pub const GUTTER: f32 = 24.;
    /// A row of an activity feed: taller than a data row, as a feed is read, not scanned.
    pub const FEED_ROW: f32 = 36.;
    pub const SIDEBAR_WIDTH: f32 = 264.;
    pub const INSPECTOR_WIDTH: f32 = 340.;
    /// Width of a field's label column.
    pub const FIELD_LABEL: f32 = 112.;
    /// The gap around and between glass sheets.
    pub const SHEET_GAP: f32 = 8.;
}

/// Whether the window is drawn in the glass style (setting `studio.window_style`).
pub fn is_glass(cx: &App) -> bool {
    cx.try_global::<StudioSettings>()
        .is_some_and(|settings| settings.studio.window_style == WindowStyle::Glass)
}

/// Surface colours by role.
pub mod surface {
    use super::*;

    /// The rule between regions and rows: a hairline when flat; on glass a dotted rule,
    /// 14% ink, which lands at about half a solid line's weight.
    pub fn rule(cx: &App) -> Hsla {
        if is_glass(cx) {
            gpui::black().opacity(0.14)
        } else {
            cx.theme().colors().border_variant.opacity(0.6)
        }
    }

    /// A recessed area inside a container (an empty list, a code block on glass): a darkening
    /// rather than a fill, so the glass still shows through.
    pub fn well(_cx: &App) -> Hsla {
        gpui::black().opacity(0.015)
    }

    /// The hairline of a solid item that sits on glass.
    pub fn raised_border(_cx: &App) -> Hsla {
        gpui::black().opacity(0.07)
    }

    /// Sunken areas: header strips, code blocks, a card's header. The editor background, 2%
    /// darker, as Delta tints them.
    pub fn sunken(cx: &App) -> Hsla {
        cx.theme()
            .colors()
            .editor_background
            .blend(gpui::black().opacity(0.02))
    }

    /// A tag's fill: a faint ink, borderless, so a row of tags reads as one quiet band.
    pub fn tag(cx: &App) -> Hsla {
        cx.theme().colors().ghost_element_hover.opacity(0.7)
    }

    /// A selected row on chrome (the sidebar): a pressed pill, 5% ink, in both styles.
    pub fn selected_chrome(cx: &App) -> Hsla {
        if is_glass(cx) {
            gpui::black().opacity(0.05)
        } else {
            cx.theme().colors().ghost_element_hover
        }
    }

    /// A selected row in a document (tables, lists): a light grey in both styles; white on a
    /// white document would vanish.
    pub fn selected_document(cx: &App) -> Hsla {
        cx.theme().colors().ghost_element_hover.opacity(0.8)
    }

    /// A hovered row: half the selected fill.
    pub fn hover(cx: &App) -> Hsla {
        if is_glass(cx) {
            gpui::black().opacity(0.025)
        } else {
            cx.theme().colors().ghost_element_hover.opacity(0.5)
        }
    }

    /// Status dots: failed, running, done, idle.
    pub fn status(status: super::Status) -> Hsla {
        let rgb = match status {
            super::Status::Failed => 0xff5e1a,
            super::Status::Running => 0xf5c518,
            super::Status::Done => 0x8fd49a,
            super::Status::Idle => 0xc8c8c8,
        };
        gpui::rgb(rgb).into()
    }

    /// The glass window's base, under [`super::wash`].
    pub fn page(cx: &App) -> Hsla {
        cx.theme().colors().editor_background
    }

    /// Kept for callers that paint the wash as one background: mint to sky.
    pub fn wash(cx: &App) -> Background {
        let base = cx.theme().colors().editor_background;
        gpui::linear_gradient(
            135.,
            gpui::linear_color_stop(base.blend(gpui::hsla(0.40, 0.45, 0.65, 0.10)), 0.),
            gpui::linear_color_stop(base.blend(gpui::hsla(0.60, 0.70, 0.70, 0.08)), 1.),
        )
    }
}

/// Elevation as layered transparent shadows (better-ui: shadows for elevation, borders for
/// structure).
pub mod elevation {
    use super::*;

    fn shadow(alpha: f32, y: f32, blur: f32, spread: f32) -> BoxShadow {
        BoxShadow {
            color: gpui::black().opacity(alpha),
            offset: point(px(0.), px(y)),
            blur_radius: px(blur),
            spread_radius: px(spread),
            inset: false,
        }
    }

    fn rim(alpha: f32) -> BoxShadow {
        BoxShadow {
            color: gpui::white().opacity(alpha),
            offset: point(px(0.), px(0.)),
            blur_radius: px(0.),
            spread_radius: px(1.),
            inset: true,
        }
    }

    /// A solid item on glass: only a contact shadow.
    pub fn raised() -> Vec<BoxShadow> {
        vec![shadow(0.04, 1., 2., 0.)]
    }

    /// A glass sheet: an inner rim and a soft downward shadow.
    pub fn sheet() -> Vec<BoxShadow> {
        vec![
            rim(0.7),
            shadow(0.04, 1., 2., 0.),
            shadow(0.11, 12., 28., -9.),
        ]
    }

    /// Small floating glass controls (pills, chips): a shorter, tighter shadow.
    pub fn float() -> Vec<BoxShadow> {
        vec![
            rim(0.7),
            shadow(0.05, 1., 2., 0.),
            shadow(0.12, 6., 14., -6.),
        ]
    }

    /// Popovers and toasts.
    pub fn overlay() -> Vec<BoxShadow> {
        vec![shadow(0.06, 2., 4., 0.), shadow(0.10, 12., 32., -6.)]
    }
}

/// A row's state, drawn as a status dot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Failed,
    Running,
    Done,
    Idle,
}

/// A status dot.
/// A node type's colour as a small dot, centred in the width of a type mark so rows with
/// either line up: for rows whose context already names the type, where a lettered mark would
/// repeat it.
pub fn type_dot(type_name: &str) -> Div {
    div()
        .size(px(16.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .child(
            div()
                .size(px(7.))
                .rounded_full()
                .bg(studio_theme::type_color(type_name)),
        )
}

pub fn status_dot(status: Status) -> Div {
    div()
        .size(px(7.))
        .flex_none()
        .rounded_full()
        .bg(surface::status(status))
}

/// The glass window's wash, painted once behind everything: three soft fields of colour (warm
/// at the top right, green at the bottom left, blue at the bottom right), low enough to tint the
/// glass over them rather than colour the page. They would be radial gradients; GPUI has
/// linear ones, so each field fades from its corner.
pub fn wash() -> Div {
    let field = |angle: f32, color: Hsla| {
        div().absolute().inset_0().bg(gpui::linear_gradient(
            angle,
            gpui::linear_color_stop(color, 0.),
            gpui::linear_color_stop(color.opacity(0.), 0.55),
        ))
    };
    div()
        .absolute()
        .inset_0()
        .child(field(225., gpui::rgba(0xff966e1f).into()))
        .child(field(45., gpui::rgba(0x7dcd9628).into()))
        .child(field(315., gpui::rgba(0x78a5f01f).into()))
}

/// Text roles. Body text is `Small`; metadata and labels `XSmall`; one weight change per row.
pub mod text {
    use super::*;

    pub fn title(text: impl Into<SharedString>) -> Label {
        Label::new(text.into()).weight(gpui::FontWeight::MEDIUM)
    }

    pub fn body(text: impl Into<SharedString>) -> Label {
        Label::new(text.into()).size(LabelSize::Small)
    }

    pub fn muted(text: impl Into<SharedString>) -> Label {
        Label::new(text.into())
            .size(LabelSize::Small)
            .color(Color::Muted)
    }

    pub fn meta(text: impl Into<SharedString>) -> Label {
        Label::new(text.into())
            .size(LabelSize::XSmall)
            .color(Color::Muted)
    }

    pub fn code(text: impl Into<SharedString>, cx: &App) -> Label {
        Label::new(text.into())
            .size(LabelSize::XSmall)
            .buffer_font(cx)
    }
}

// ---- components ----

/// A quiet section label, as the sidebar's "Types" or "Branches".
pub fn section_label(title: impl Into<SharedString>) -> Div {
    div()
        .px(px(space::LG))
        .pt(px(space::BETWEEN))
        .pb(px(space::SM))
        .child(text::meta(title))
}

/// A section label that folds the rows under it ("Archived 3 ›"), on the same edge as
/// [`section_label`].
pub fn section_disclosure(
    id: impl Into<ElementId>,
    title: impl Into<SharedString>,
    count: usize,
    open: bool,
) -> Stateful<Div> {
    h_flex()
        .id(id.into())
        .px(px(space::LG))
        .pt(px(space::BETWEEN))
        .pb(px(space::SM))
        .gap(px(space::XS))
        .items_center()
        .cursor_pointer()
        .child(text::meta(title))
        .child(text::meta(count.to_string()))
        .child(
            Icon::new(if open {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            })
            .size(IconSize::XSmall)
            .color(Color::Muted),
        )
}

/// A panel's title line: its quiet label, and its actions on the right (search, new) when it
/// has any, on the label's baseline.
pub fn section_header(title: impl Into<SharedString>, actions: Option<AnyElement>) -> Div {
    match actions {
        None => section_label(title),
        Some(actions) => h_flex()
            .pl(px(space::LG))
            .pr(px(space::MD))
            .pt(px(space::BETWEEN - space::XS))
            .pb(px(space::XS))
            .items_center()
            .child(text::meta(title))
            .child(div().flex_1())
            .child(actions),
    }
}

type TooltipBuilder = Box<dyn Fn(&mut Window, &mut App) -> gpui::AnyView + 'static>;
type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;
/// What a setting row's reset button does.
pub type SettingReset = ClickHandler;

/// A sidebar row: an icon or dot, a label, a trailing count; inset with rounded corners, and
/// a pill when selected (grey when flat, raised white on glass).
#[derive(IntoElement)]
pub struct SidebarRow {
    id: ElementId,
    start: Option<AnyElement>,
    label: SharedString,
    label_color: Color,
    end: Option<AnyElement>,
    /// Shown in place of `end` while the row is hovered: pin, archive.
    actions: Option<AnyElement>,
    selected: bool,
    tooltip: Option<TooltipBuilder>,
    on_click: Option<ClickHandler>,
}

/// The hover group of a sidebar row, for its actions.
const SIDEBAR_ROW_GROUP: &str = "sidebar-row";

impl SidebarRow {
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            start: None,
            label: label.into(),
            label_color: Color::Default,
            end: None,
            actions: None,
            selected: false,
            tooltip: None,
            on_click: None,
        }
    }

    /// Small buttons that take the trailing value's place while the row is hovered.
    pub fn actions(mut self, actions: impl IntoElement) -> Self {
        self.actions = Some(actions.into_any_element());
        self
    }

    pub fn start(mut self, start: impl IntoElement) -> Self {
        self.start = Some(start.into_any_element());
        self
    }

    pub fn end(mut self, end: impl IntoElement) -> Self {
        self.end = Some(end.into_any_element());
        self
    }

    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    pub fn muted(mut self, muted: bool) -> Self {
        if muted {
            self.label_color = Color::Muted;
        }
        self
    }

    pub fn tooltip(mut self, tooltip: impl Into<SharedString>) -> Self {
        let tooltip: SharedString = tooltip.into();
        self.tooltip = Some(Box::new(Tooltip::text(tooltip)));
        self
    }

    pub fn tooltip_fn(
        mut self,
        tooltip: impl Fn(&mut Window, &mut App) -> gpui::AnyView + 'static,
    ) -> Self {
        self.tooltip = Some(Box::new(tooltip));
        self
    }

    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Box::new(handler));
        self
    }
}

impl RenderOnce for SidebarRow {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let (selected_bg, hover_bg, glass) = (
            surface::selected_chrome(cx),
            surface::hover(cx),
            is_glass(cx),
        );
        h_flex()
            .id(self.id)
            .relative()
            .h(px(size::SIDEBAR_ROW))
            .mx(px(space::SM))
            .px(px(space::SM))
            .gap(px(space::MD))
            .items_center()
            .rounded(px(radius::SM))
            .cursor_pointer()
            .when(self.selected, |row| {
                row.bg(selected_bg)
                    .when(glass, |row| row.shadow(elevation::raised()))
            })
            .when(!self.selected, |row| {
                row.hover(move |style| style.bg(hover_bg))
            })
            .when_some(self.tooltip, |row, tooltip| row.tooltip(tooltip))
            .children(self.start)
            .child(
                Label::new(self.label)
                    .size(LabelSize::Small)
                    .color(self.label_color)
                    .single_line()
                    .truncate(),
            )
            .child(div().flex_1())
            .map(|row| match self.actions {
                Some(actions) => row
                    .group(SIDEBAR_ROW_GROUP)
                    .children(self.end.map(|end| {
                        div()
                            .flex_none()
                            .group_hover(SIDEBAR_ROW_GROUP, |style| style.invisible())
                            .child(end)
                    }))
                    .child(
                        h_flex()
                            .absolute()
                            .right(px(space::XS))
                            .top_0()
                            .bottom_0()
                            .items_center()
                            .visible_on_hover(SIDEBAR_ROW_GROUP)
                            .child(actions),
                    ),
                None => row.children(self.end),
            })
            .when_some(self.on_click, |row, handler| row.on_click(handler))
    }
}

/// A count or other trailing value in a sidebar row.
pub fn sidebar_count(text: impl Into<SharedString>) -> Label {
    text::meta(text)
}

/// An empty sidebar section: one dim row with an icon and a word or two; how to fill it goes
/// in the tooltip.
pub fn sidebar_placeholder(
    id: impl Into<ElementId>,
    icon: IconName,
    label: impl Into<SharedString>,
    tooltip: impl Into<SharedString>,
) -> Stateful<Div> {
    let tooltip: SharedString = tooltip.into();
    h_flex()
        .id(id.into())
        .h(px(size::SIDEBAR_ROW))
        .mx(px(space::SM))
        .px(px(space::SM))
        .gap(px(space::MD))
        .items_center()
        .tooltip(Tooltip::text(tooltip))
        .child(
            Icon::new(icon)
                .size(IconSize::XSmall)
                .color(Color::Disabled),
        )
        .child(
            Label::new(label.into())
                .size(LabelSize::Small)
                .color(Color::Disabled),
        )
}

/// A one-line waiting state inside a sidebar section ("Reading types…").
pub fn sidebar_note(label: impl Into<SharedString>) -> Div {
    // A row tall at least, and wrapping when the note is longer than the sidebar is wide.
    div()
        .mx(px(space::SM))
        .px(px(space::SM))
        .py(px(space::XS))
        .min_h(px(size::SIDEBAR_ROW))
        .flex()
        .items_center()
        .child(text::muted(label))
}

/// A surface's toolbar: fixed height, the column's edge inset, a rule below.
pub fn toolbar(cx: &App) -> Div {
    h_flex()
        .h(px(size::TOOLBAR))
        .flex_none()
        .px(px(space::EDGE))
        .gap(px(space::CONTROLS))
        .items_center()
        .border_b_1()
        .border_color(surface::rule(cx))
}

/// A short vertical hairline between groups in a strip (Delta's footer: context | facts).
pub fn rule_v(cx: &App) -> Div {
    div().w(px(1.)).h(px(16.)).flex_none().bg(surface::rule(cx))
}

/// A panel's tab in a dock's header strip: its icon and title, the active one dark and, when
/// the dock has a choice of panels, on a pill.
pub fn title_tab(
    id: impl Into<ElementId>,
    icon: IconName,
    title: impl Into<SharedString>,
    selected: bool,
    pill: bool,
    cx: &App,
) -> Stateful<Div> {
    h_flex()
        .id(id.into())
        .h(px(26.))
        .px(px(space::MD))
        .gap(px(space::SM))
        .items_center()
        .rounded(px(radius::SM))
        .when(selected && pill, |tab| tab.bg(surface::selected_chrome(cx)))
        .cursor_pointer()
        .child(Icon::new(icon).size(IconSize::XSmall).color(Color::Muted))
        .child(Label::new(title).size(LabelSize::Small).color(if selected {
            Color::Default
        } else {
            Color::Muted
        }))
}

/// A document's footer strip.
pub fn footer(cx: &App) -> Div {
    h_flex()
        .h(px(size::STRIP))
        .flex_none()
        .px(px(space::EDGE))
        .gap(px(space::XL))
        .items_center()
        .border_t_1()
        .border_color(surface::rule(cx))
}

/// Metadata as an icon and a short value (a count, a commit, a branch), instead of a sentence.
pub fn meta(icon: IconName, value: impl Into<SharedString>) -> Div {
    h_flex()
        .gap(px(space::XS))
        .items_center()
        .flex_none()
        .child(Icon::new(icon).size(IconSize::XSmall).color(Color::Muted))
        .child(text::meta(value).single_line())
}

/// [`meta`] with the value in the buffer font, for commit ids and keys.
pub fn meta_code(icon: IconName, value: impl Into<SharedString>, cx: &App) -> Div {
    h_flex()
        .gap(px(space::XS))
        .items_center()
        .flex_none()
        .child(Icon::new(icon).size(IconSize::XSmall).color(Color::Muted))
        .child(text::code(value, cx).color(Color::Muted).single_line())
}

/// A button in a toolbar, strip, panel or dialog: Zed's medium size (28 px tall, an 8 px
/// inset) with a small label, so its hover and pressed fills have room around the label. A
/// button inside a dense row (a pill segment, a tree row, a tab) keeps Zed's compact default.
pub fn button(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Button {
    Button::new(id, label)
        .size(ButtonSize::Medium)
        .label_size(LabelSize::Small)
}

/// [`button`] for an icon alone: a 28 px square.
pub fn icon_button(id: impl Into<ElementId>, icon: IconName) -> IconButton {
    IconButton::new(id, icon)
        .size(ButtonSize::Medium)
        .icon_size(IconSize::Small)
}

/// The text of a single-line field that sits among `Small` labels (a filter value, a search
/// box): an editor's text at the label's size rather than the body size.
pub fn field_text(cx: &App) -> gpui::TextStyleRefinement {
    gpui::TextStyleRefinement {
        font_size: Some(ui::TextSize::Small.rems(cx).into()),
        ..Default::default()
    }
}

/// An applied filter as a segmented pill: the field with its icon, the test, the value, and a
/// remove button, split by hairlines.
pub fn filter_pill(
    id: impl Into<ElementId>,
    icon: IconName,
    field: impl Into<SharedString>,
    test: impl Into<SharedString>,
    value: Option<SharedString>,
    on_remove: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> Stateful<Div> {
    let id: ElementId = id.into();
    let rule = surface::rule(cx);
    let divider = move || div().w(px(1.)).h_full().flex_none().bg(rule);
    let segment = || {
        h_flex()
            .h_full()
            .px(px(space::MD))
            .gap(px(space::XS))
            .items_center()
            .flex_none()
    };
    h_flex()
        .id(id.clone())
        .h(px(26.))
        .flex_none()
        .rounded_full()
        .border_1()
        .border_color(surface::raised_border(cx))
        .bg(cx.theme().colors().editor_background)
        .shadow(elevation::raised())
        .overflow_hidden()
        .child(
            segment()
                .child(Icon::new(icon).size(IconSize::XSmall).color(Color::Muted))
                .child(Label::new(field.into()).size(LabelSize::Small)),
        )
        .child(divider())
        .child(segment().child(text::muted(test).size(LabelSize::Small)))
        .when_some(value, |pill, value| {
            pill.child(divider()).child(
                segment().max_w(px(220.)).child(
                    Label::new(value)
                        .size(LabelSize::Small)
                        .single_line()
                        .truncate(),
                ),
            )
        })
        .child(divider())
        .child(
            div()
                .h_full()
                .px(px(space::XS))
                .flex()
                .items_center()
                .child(
                    IconButton::new("remove", IconName::Close)
                        .icon_size(IconSize::XSmall)
                        .icon_color(Color::Muted)
                        .tooltip(Tooltip::text("Remove this filter"))
                        .on_click(on_remove),
                ),
        )
}

/// The widest a [`tag`] grows before its label truncates.
pub const TAG_MAX_WIDTH: f32 = 160.;

/// A tag: a name in a quiet pill, as a table shows a row's neighbours. The UI font, unlike
/// `ui::Chip`, which is for code-like values such as enum members.
pub fn tag(label: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .flex_none()
        .max_w(px(TAG_MAX_WIDTH))
        .h(px(18.))
        .px(px(space::SM))
        .flex()
        .items_center()
        .rounded(px(radius::XS))
        .bg(surface::tag(cx))
        .child(
            Label::new(label.into())
                .size(LabelSize::XSmall)
                .single_line()
                .truncate(),
        )
}

/// A segmented control: a sunken track holding [`segment`]s, for a choice of two to four.
/// Concentric: the track's radius is a segment's plus its padding.
pub fn segmented(cx: &App) -> Div {
    h_flex()
        .flex_none()
        .p(px(space::XXS))
        .gap(px(space::XXS))
        .rounded(px(radius::SM + space::XXS))
        .bg(surface::sunken(cx).blend(gpui::black().opacity(0.03)))
}

/// One option of a [`segmented`] control; the selected one is raised.
pub fn segment(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    selected: bool,
    cx: &App,
) -> Stateful<Div> {
    let hover = surface::hover(cx);
    let raised = cx.theme().colors().editor_background;
    h_flex()
        .id(id.into())
        .h(px(24.))
        .px(px(space::MD + space::XXS))
        .items_center()
        .rounded(px(radius::SM))
        .cursor_pointer()
        .when(selected, |segment| {
            segment.bg(raised).shadow(elevation::raised())
        })
        .when(!selected, |segment| {
            segment.hover(move |style| style.bg(hover))
        })
        .child(
            Label::new(label.into())
                .size(LabelSize::Small)
                .color(if selected {
                    Color::Default
                } else {
                    Color::Muted
                })
                .single_line(),
        )
}

/// A setting: its title (with a reset button once changed) and one-line description on the
/// leading side, its control on the trailing side, as Zed's settings rows.
pub fn setting_row(
    id: impl Into<ElementId>,
    title: impl Into<SharedString>,
    description: impl Into<SharedString>,
    control: impl IntoElement,
    reset: Option<SettingReset>,
) -> Stateful<Div> {
    let id: ElementId = id.into();
    h_flex()
        .id(id.clone())
        .w_full()
        .min_h(px(size::ROW + space::XL))
        .py(px(space::MD))
        .gap(px(space::XXL))
        .items_center()
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap(px(space::WITHIN))
                .child(
                    h_flex()
                        .gap(px(space::XS))
                        .child(Label::new(title.into()))
                        .when_some(reset, |title, reset| {
                            title.child(
                                IconButton::new("reset", IconName::Undo)
                                    .icon_size(IconSize::XSmall)
                                    .icon_color(Color::Muted)
                                    .tooltip(Tooltip::text("Reset to default"))
                                    .on_click(reset),
                            )
                        }),
                )
                .child(text::muted(description)),
        )
        .child(div().flex_none().child(control))
}

/// A key cap and a one-word label, as Zed's hints show keys.
pub fn key_hint(
    action: &dyn gpui::Action,
    label: impl Into<SharedString>,
    focus: &gpui::FocusHandle,
    cx: &App,
) -> Div {
    h_flex()
        .gap(px(space::XS))
        .items_center()
        .flex_none()
        .child(ui::KeyBinding::for_action_in(action, focus, cx).size(rems_from_px(11_f32)))
        .child(text::meta(label))
}

/// A modal dialog's frame: a raised sheet with its title; the body and the buttons (Cancel, then
/// the action) follow as children.
pub fn dialog(title: impl Into<SharedString>, cx: &App) -> Div {
    let colors = cx.theme().colors();
    v_flex()
        .w(px(440.))
        .p_4()
        .gap_3()
        .rounded_lg()
        .bg(colors.elevated_surface_background)
        .border_1()
        .border_color(colors.border)
        .shadow_lg()
        .child(Headline::new(title.into()).size(HeadlineSize::Small))
}

/// A bordered block of code or a command, as Delta draws edits and commands in a thread.
pub fn code_block(code: impl Into<SharedString>, cx: &App) -> Div {
    code_frame(cx).child(code.into())
}

/// [`code_block`] with syntax colours: each range takes the theme's style of that name
/// ("keyword", "string", …), as the editor colours the same text.
pub fn code_block_highlighted(
    code: impl Into<SharedString>,
    highlights: impl IntoIterator<Item = (std::ops::Range<usize>, &'static str)>,
    cx: &App,
) -> Div {
    let syntax = cx.theme().syntax();
    let highlights: Vec<_> = highlights
        .into_iter()
        .filter_map(|(range, name)| Some((range, syntax.style_for_name(name)?)))
        .collect();
    code_frame(cx).child(gpui::StyledText::new(code.into()).with_highlights(highlights))
}

fn code_frame(cx: &App) -> Div {
    let block = div()
        .w_full()
        .px(px(space::LG))
        .py(px(space::MD))
        .rounded(px(radius::SM))
        .border_1();
    let block = if is_glass(cx) {
        block
            .border_dashed()
            .border_color(surface::rule(cx))
            .bg(surface::well(cx))
    } else {
        block
            .border_color(cx.theme().colors().border_variant)
            .bg(surface::sunken(cx))
    };
    block
        .font_buffer(cx)
        .text_xs()
        .text_color(cx.theme().colors().text)
}

/// A group of [`field`]s: rows close together, so the group reads as one block.
pub fn fields() -> Div {
    v_flex().w_full().gap(px(space::WITHIN))
}

/// A field: a label column and a value, for property grids and parameters.
pub fn field(label: impl Into<SharedString>, value: impl IntoElement) -> Div {
    h_flex()
        .w_full()
        .min_h(px(24.))
        .gap(px(space::LG))
        .items_center()
        .child(
            div()
                .w(px(size::FIELD_LABEL))
                .flex_none()
                .child(text::muted(label).single_line().truncate()),
        )
        .child(div().flex_1().min_w_0().child(value))
}

/// A card, as Zed draws a tool call: a tinted header, a body and a footer of actions. Compose
/// with [`card_header`], [`card_footer`].
pub fn card(cx: &App) -> Div {
    let card = v_flex()
        .w_full()
        .rounded(px(radius::MD))
        .border_1()
        .bg(cx.theme().colors().editor_background)
        .overflow_hidden();
    if is_glass(cx) {
        // A solid item on glass: raised.
        card.border_color(surface::raised_border(cx))
            .shadow(elevation::raised())
    } else {
        card.border_color(cx.theme().colors().border.opacity(0.7))
    }
}

pub fn card_header(cx: &App) -> Div {
    h_flex()
        .w_full()
        .min_h(px(size::SIDEBAR_ROW + space::MD))
        .px(px(space::LG))
        .gap(px(space::MD))
        .items_center()
        .bg(surface::sunken(cx))
}

pub fn card_body() -> Div {
    v_flex().p(px(space::LG)).gap(px(space::LG))
}

pub fn card_footer(cx: &App) -> Div {
    h_flex()
        .w_full()
        .px(px(space::MD))
        .py(px(space::SM))
        .gap(px(space::XS))
        .items_center()
        .border_t_1()
        .border_color(cx.theme().colors().border.opacity(0.7))
}

/// An empty state, as Zed's panels draw one: an icon, a title of two or three words, then the
/// actions that fill the space. Explanations go in the actions' tooltips.
pub fn empty_state(icon: IconName, title: impl Into<SharedString>) -> Div {
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .gap(px(space::LG))
        .p(px(space::PAGE))
        .child(
            Icon::new(icon)
                .size(IconSize::Medium)
                .color(Color::Disabled),
        )
        .child(Label::new(title.into()).color(Color::Muted))
}

/// A raised glass sheet: a translucent fill over the wash, a light outline, and layered soft
/// shadows instead of a border.
pub fn sheet(_cx: &App) -> Div {
    div()
        .rounded(px(radius::LG))
        .overflow_hidden()
        .bg(gpui::rgba(0xfcfbfccc))
        .shadow(elevation::sheet())
}

/// The rule under a list row: a hairline, dotted on glass.
pub fn row_rule(row: Div, cx: &App) -> Div {
    let row = row.border_b_1().border_color(surface::rule(cx));
    if is_glass(cx) {
        row.border_dashed()
    } else {
        row
    }
}

/// One entry of an activity feed, written as structure rather than a sentence: a status dot,
/// who (icon and name), what they did (muted), the object (underlined), where (icon and name),
/// and when, right-aligned.
#[derive(IntoElement)]
pub struct ActivityRow {
    id: ElementId,
    status: Status,
    actor_icon: IconName,
    actor: SharedString,
    verb: SharedString,
    object: Option<SharedString>,
    preposition: Option<SharedString>,
    place_icon: Option<IconName>,
    place: Option<SharedString>,
    when: SharedString,
    tooltip: Option<SharedString>,
}

impl ActivityRow {
    pub fn new(
        id: impl Into<ElementId>,
        actor_icon: IconName,
        actor: impl Into<SharedString>,
        verb: impl Into<SharedString>,
        when: impl Into<SharedString>,
    ) -> Self {
        Self {
            id: id.into(),
            status: Status::Done,
            actor_icon,
            actor: actor.into(),
            verb: verb.into(),
            object: None,
            preposition: None,
            place_icon: None,
            place: None,
            when: when.into(),
            tooltip: None,
        }
    }

    pub fn status(mut self, status: Status) -> Self {
        self.status = status;
        self
    }

    pub fn object(mut self, object: impl Into<SharedString>) -> Self {
        self.object = Some(object.into());
        self
    }

    pub fn place(
        mut self,
        preposition: impl Into<SharedString>,
        icon: IconName,
        place: impl Into<SharedString>,
    ) -> Self {
        self.preposition = Some(preposition.into());
        self.place_icon = Some(icon);
        self.place = Some(place.into());
        self
    }

    pub fn tooltip(mut self, tooltip: impl Into<SharedString>) -> Self {
        self.tooltip = Some(tooltip.into());
        self
    }
}

impl RenderOnce for ActivityRow {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        // An object link: a faint underline, 15% ink.
        let underline = cx.theme().colors().text.opacity(0.15);
        let row = h_flex()
            .w_full()
            .h(px(size::FEED_ROW))
            .px(px(space::EDGE))
            .gap(px(space::SM))
            .items_center()
            .child(status_dot(self.status))
            .child(
                Icon::new(self.actor_icon)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .child(text::body(self.actor).single_line())
            .child(text::muted(self.verb).single_line())
            .children(self.object.map(|object| {
                div()
                    .border_b_1()
                    .border_color(underline)
                    .child(text::body(object).single_line())
            }))
            .children(self.preposition.map(|p| text::muted(p).single_line()))
            .children(
                self.place_icon
                    .map(|icon| Icon::new(icon).size(IconSize::XSmall).color(Color::Muted)),
            )
            .children(
                self.place
                    .map(|place| text::body(place).single_line().truncate()),
            )
            .child(div().flex_1())
            .child(text::meta(self.when).single_line());
        let row = row_rule(row, cx);
        div()
            .id(self.id)
            .when_some(self.tooltip, |d, tooltip| d.tooltip(Tooltip::text(tooltip)))
            .child(row)
    }
}

// ---- components from the reference frames ----

/// An icon on a small tile, as a card or entry leads with one.
pub fn tile(icon: IconName) -> Div {
    div()
        .size(px(28.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(radius::SM))
        .bg(gpui::black().opacity(0.04))
        .child(Icon::new(icon).size(IconSize::Small).color(Color::Muted))
}

/// Metadata items joined by a middle dot: "123 runs · last run 2m ago · by act-dev".
pub fn meta_row(items: impl IntoIterator<Item = AnyElement>) -> Div {
    let mut row = h_flex().gap(px(space::SM)).items_center().min_w_0();
    for (index, item) in items.into_iter().enumerate() {
        if index > 0 {
            row = row.child(
                Label::new("·")
                    .size(LabelSize::XSmall)
                    .color(Color::Disabled),
            );
        }
        row = row.child(item);
    }
    row
}

/// A section label with a dotted rule running to the edge, as a feed groups by date.
pub fn section_rule(title: impl Into<SharedString>, cx: &App) -> Div {
    h_flex()
        .w_full()
        .gap(px(space::LG))
        .items_center()
        .child(text::meta(title))
        .child(
            div()
                .flex_1()
                .h(px(1.))
                .border_t_1()
                .border_dashed()
                .border_color(surface::rule(cx)),
        )
}

/// A count in a soft tinted pill: pale status tints, keyed by a hue so the same type
/// gets the same colour everywhere.
pub fn number_badge(value: impl Into<SharedString>, hue: f32) -> Div {
    div()
        .h(px(18.))
        .min_w(px(18.))
        .px(px(space::SM))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(gpui::hsla(hue, 0.70, 0.92, 1.))
        .child(
            Label::new(value.into())
                .size(LabelSize::XSmall)
                .color(Color::Custom(gpui::hsla(hue, 0.55, 0.40, 1.))),
        )
}

/// A rounded-full control with a hairline: a picker in a composer ("Claude ▾").
pub fn pill(id: impl Into<ElementId>, cx: &App) -> Stateful<Div> {
    h_flex()
        .id(id.into())
        .h(px(26.))
        .px(px(space::MD))
        .gap(px(space::XS))
        .items_center()
        .rounded_full()
        .border_1()
        .border_color(cx.theme().colors().border_variant)
        .cursor_pointer()
        .hover(|style| style.bg(gpui::black().opacity(0.03)))
}

/// A circular icon button: attach, send.
pub fn round_button(
    id: impl Into<ElementId>,
    icon: IconName,
    filled: bool,
    cx: &App,
) -> Stateful<Div> {
    let colors = cx.theme().colors();
    div()
        .id(id.into())
        .size(px(28.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .cursor_pointer()
        .map(|button| {
            if filled {
                button.bg(colors.text).child(
                    Icon::new(icon)
                        .size(IconSize::Small)
                        .color(Color::Custom(colors.editor_background)),
                )
            } else {
                button
                    .border_1()
                    .border_color(colors.border_variant)
                    .hover(|style| style.bg(gpui::black().opacity(0.03)))
                    .child(Icon::new(icon).size(IconSize::Small).color(Color::Muted))
            }
        })
}

/// What a composer is about: a tile, a title and a muted line, in a pressed pill.
pub fn context_chip(
    icon: IconName,
    title: impl Into<SharedString>,
    detail: impl Into<SharedString>,
) -> Div {
    h_flex()
        .gap(px(space::MD))
        .p(px(space::SM))
        .pr(px(space::LG))
        .rounded(px(radius::MD))
        .bg(gpui::black().opacity(0.035))
        .child(tile(icon))
        .child(
            v_flex()
                .child(text::body(title).single_line())
                .child(text::meta(detail).single_line()),
        )
}

/// One entry of a timeline: a hollow dot on a dotted rail, an optional disclosure, a bold
/// subject and a muted verb, then where it came from and when, right-aligned. Nested content
/// (an expanded group) goes under it, on the rail.
#[derive(IntoElement)]
pub struct TimelineEntry {
    id: ElementId,
    subject: SharedString,
    verb: SharedString,
    expanded: Option<bool>,
    source_icon: Option<IconName>,
    source: Option<SharedString>,
    when: SharedString,
    lead: Option<AnyElement>,
    nested: Option<AnyElement>,
    last: bool,
}

impl TimelineEntry {
    pub fn new(
        id: impl Into<ElementId>,
        subject: impl Into<SharedString>,
        verb: impl Into<SharedString>,
        when: impl Into<SharedString>,
    ) -> Self {
        Self {
            id: id.into(),
            subject: subject.into(),
            verb: verb.into(),
            expanded: None,
            source_icon: None,
            source: None,
            when: when.into(),
            lead: None,
            nested: None,
            last: false,
        }
    }

    /// Show a disclosure: `true` expanded, `false` collapsed.
    pub fn disclosure(mut self, expanded: bool) -> Self {
        self.expanded = Some(expanded);
        self
    }

    pub fn source(mut self, icon: IconName, source: impl Into<SharedString>) -> Self {
        self.source_icon = Some(icon);
        self.source = Some(source.into());
        self
    }

    /// An icon or logo before the subject, in place of a disclosure.
    pub fn lead(mut self, lead: impl IntoElement) -> Self {
        self.lead = Some(lead.into_any_element());
        self
    }

    pub fn nested(mut self, nested: impl IntoElement) -> Self {
        self.nested = Some(nested.into_any_element());
        self
    }

    pub fn last(mut self, last: bool) -> Self {
        self.last = last;
        self
    }
}

impl RenderOnce for TimelineEntry {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let rule = surface::rule(cx);
        let rail = div()
            .w(px(size::GUTTER))
            .flex_none()
            .relative()
            .flex()
            .justify_center()
            // The dotted rail runs the entry's height, except below the last entry.
            .when(!self.last, |rail| {
                rail.child(
                    div()
                        .absolute()
                        .top(px(18.))
                        .bottom_0()
                        .w(px(1.))
                        .border_l_1()
                        .border_dashed()
                        .border_color(rule),
                )
            })
            .child(
                div()
                    .mt(px(11.))
                    .size(px(7.))
                    .rounded_full()
                    .border_1()
                    .border_color(rule)
                    .bg(cx.theme().colors().editor_background),
            );
        let header = h_flex()
            .h(px(30.))
            .gap(px(space::SM))
            .items_center()
            .children(self.expanded.map(|expanded| {
                Icon::new(if expanded {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .size(IconSize::XSmall)
                .color(Color::Muted)
            }))
            .children(self.lead)
            .child(
                text::body(self.subject)
                    .weight(gpui::FontWeight::MEDIUM)
                    .single_line(),
            )
            .child(text::muted(self.verb).single_line())
            .child(div().flex_1())
            .child(meta_row(
                self.source
                    .map(|source| {
                        h_flex()
                            .gap(px(space::XS))
                            .child(text::meta("From"))
                            .children(self.source_icon.map(|icon| {
                                Icon::new(icon).size(IconSize::XSmall).color(Color::Muted)
                            }))
                            .child(text::meta(source).single_line())
                            .into_any_element()
                    })
                    .into_iter()
                    .chain(std::iter::once(text::meta(self.when).into_any_element())),
            ));
        h_flex()
            .id(self.id)
            .w_full()
            // Stretch, so the rail spans the whole entry, nested content included.
            .items_stretch()
            .child(rail)
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .pb(px(space::XS))
                    .child(header)
                    .children(
                        self.nested
                            .map(|nested| div().pb(px(space::MD)).child(nested)),
                    ),
            )
    }
}
