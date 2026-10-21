//! Studio's design system (spec 002, "Visual language"): tokens, then components built only
//! from tokens. Surfaces compose these; they do not pick their own paddings, radii, shadows or
//! colours. The gallery tab (`studio::OpenDesignSystem`) renders every piece in both window
//! styles, and changes are judged there first.
//!
//! Sources: Delta's window (measured from the mock on delta.dev) and Zed's `ui` components;
//! layout and polish rules: group with space, not lines; concentric radii; shadows for
//! elevation.

use gpui::{
    AnyElement, App, Bounds, BoxShadow, ClickEvent, Corners, Div, ElementId, Hsla, MouseButton,
    Pixels, SharedString, Stateful, Window, point,
};
use studio_settings::{StudioSettings, WindowStyle};
use ui::prelude::*;
use ui::{CommonAnimationExt as _, Tooltip, rems_from_px};

pub use crate::button::{Button, IconButton, Shape, Variant};
pub use crate::menu::{MenuRow, menu_header, menu_separator, menu_surface};
pub use crate::modal::{
    ModalHeader, action_button, cancel_button, modal, modal_body, modal_footer, write_target,
};

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
    /// Inside a control's pill, either side of its content: a medium button, a field.
    pub const CONTROL_INSET: f32 = 10.;
    /// A modal's inset: its header, body and footer from its edge.
    pub const MODAL: f32 = XXL;
    /// Between a modal's sections (its header and body, its body's groups).
    pub const MODAL_STACK: f32 = LG;
    /// Padding of a document page (settings, storage, empty states).
    pub const PAGE: f32 = XXL;
}

/// Corner radii. Every radius comes from one base, the person's corner radius setting
/// (`studio.corner_radius`, 10 px by default), through [`radii`]; nothing picks its own.
/// Nested surfaces stay concentric: a surface holding rounded children has their radius plus
/// the gap between its edge and theirs ([`radius::around`]); a child takes its container's
/// radius minus that gap ([`radius::within`]). The same radius on both reads as a mistake: the
/// gap is narrower along the sides than at the corners. Buttons, fields, tabs, pills and
/// floating bars are round at their ends (`rounded_full`) whatever the base.
pub mod radius {
    /// The base when no settings are installed: the setting's default.
    pub const DEFAULT_BASE: f32 = 10.;

    /// The radius of a surface whose children have radius `inner` and sit `gap` inside its
    /// edge (its border and padding).
    pub const fn around(inner: f32, gap: f32) -> f32 {
        inner + gap
    }

    /// The radius of a child `gap` inside a surface of radius `outer`; square when the gap is
    /// wider than the radius.
    pub const fn within(outer: f32, gap: f32) -> f32 {
        if outer > gap { outer - gap } else { 0. }
    }

    /// A fill flush with a bordered surface's edge (a card's header, a modal's footer): the
    /// surface's radius less its hairline. GPUI clips children to a rectangle, not to their
    /// container's corners, so such a fill rounds its own.
    pub const fn flush(outer: f32) -> f32 {
        within(outer, super::size::HAIRLINE)
    }
}

/// The radii by role, for one base. The gaps between nested surfaces are fixed, so each
/// radius is the base plus or minus the gap it nests across (as Tower's `--radius-sm` is
/// `--radius` less 4 px), and they stay concentric at any base. A base of 0 squares every
/// corner.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Radii {
    /// Key caps, chips, tags, code blocks, the rows of a [`row_list`]: a card's radius less
    /// [`space::SM`], as a chip sits that far inside a card.
    pub xs: f32,
    /// Rows, tiles, inline frames: a card's radius less [`space::XS`].
    pub sm: f32,
    /// Cards, menus and popovers: the base.
    pub md: f32,
    /// Glass sheets, which hold cards [`space::XS`] inside their edge.
    pub lg: f32,
    /// Modals and the command palette, the largest surfaces that float: a card's radius and
    /// [`space::SM`].
    pub modal: f32,
}

impl Radii {
    pub fn new(base: f32) -> Self {
        let md = base.max(0.).round();
        let grow = |gap: f32| if md > 0. { radius::around(md, gap) } else { 0. };
        Self {
            xs: radius::within(md, space::SM),
            sm: radius::within(md, space::XS),
            md,
            lg: grow(space::XS),
            modal: grow(space::SM),
        }
    }

    /// A glass sheet (a dock, the document column): the largest radius but a modal's. A
    /// document's fill that reaches the sheet's bottom takes it too ([`sheet`]).
    pub fn sheet(&self) -> f32 {
        self.lg
    }

    /// A context menu or popover: a card's radius.
    pub fn menu(&self) -> f32 {
        self.md
    }

    /// A menu's rows, inside its hairline and [`space::XS`] of padding.
    pub fn menu_item(&self) -> f32 {
        radius::within(self.menu(), space::XS + size::HAIRLINE)
    }

    /// The command palette's results: a solid card inside its frame's hairline and padding.
    pub fn palette_list(&self) -> f32 {
        radius::within(self.modal, space::XS + size::HAIRLINE)
    }

    /// A palette row, inside the results card's hairline and padding.
    pub fn palette_row(&self) -> f32 {
        radius::within(self.palette_list(), space::XS + size::HAIRLINE)
    }

    /// A [`float_bar`]: concentric around the `sm` controls set 3 px inside its edge.
    pub fn bar(&self) -> f32 {
        radius::around(self.sm, (size::CONTROL - size::BAR_BUTTON) / 2.)
    }

    /// A [`row_list`]: concentric around its `xs` rows, inside its hairline and padding.
    pub fn row_list(&self) -> f32 {
        radius::around(self.xs, space::XS + size::HAIRLINE)
    }
}

/// The radii for the person's corner radius setting.
pub fn radii(cx: &App) -> Radii {
    Radii::new(
        cx.try_global::<StudioSettings>()
            .map_or(radius::DEFAULT_BASE, |settings| {
                settings.studio.corner_radius as f32
            }),
    )
}

/// Fixed sizes.
pub mod size {
    /// Every column's header strip (its tabs or its title), so the rules under them line up
    /// across the window: Zed's tab bar (`Tab::container_height`).
    pub const HEADER_STRIP: f32 = 32.;
    /// Every column's footer strip, and a bar across a document (a write to confirm), so the
    /// rules over them line up.
    pub const STRIP: f32 = 40.;
    /// The window's status bar, after Zed's: the docks' panel buttons and the open tab's facts,
    /// a [`BAR_BUTTON`] with 4 px above and below it.
    pub const STATUS_BAR: f32 = 30.;
    /// A surface's toolbar.
    pub const TOOLBAR: f32 = 48.;
    /// A data row and a data header: dense enough to scan a page at a glance, with room for a
    /// chip (an enum value, a relationship) inside a row.
    pub const ROW: f32 = 28.;
    pub const HEADER: f32 = 30.;
    /// A sidebar row, and a row of a [`super::row_list`].
    pub const SIDEBAR_ROW: f32 = 26.;
    /// The slot a sidebar row's icon or type mark sits in, so every label of a list starts on
    /// one edge whatever leads it.
    pub const ROW_ICON: f32 = 16.;
    /// A control: a medium button, a field, a segmented track, a floating bar, a tile.
    pub const CONTROL: f32 = 28.;
    /// A button inside a floating bar ([`super::bar_button`]): Zed's default size, 3 px inside
    /// the bar's edge.
    pub const BAR_BUTTON: f32 = 22.;
    /// A button inside a row, a card or beside a value: a tag's height, so a row action sits
    /// in the row as its chips do.
    pub const ROW_BUTTON: f32 = TAG;
    /// The large button, one step above a control.
    pub const LARGE_BUTTON: f32 = 32.;
    /// A pill: a filter, a composer's picker, a dock's title tab.
    pub const PILL: f32 = 26.;
    /// A thread's or feed's gutter, where each entry's icon sits.
    pub const GUTTER: f32 = 24.;
    /// A row of the command palette: a pick list is read once, so a little taller than a menu's.
    pub const PALETTE_ROW: f32 = 32.;
    /// A row of an activity feed: taller than a data row, as a feed is read, not scanned.
    pub const FEED_ROW: f32 = 36.;
    pub const SIDEBAR_WIDTH: f32 = 264.;
    pub const INSPECTOR_WIDTH: f32 = 340.;
    /// The widest a state's detail line runs before it wraps (`state_detail`).
    pub const STATE_DETAIL_WIDTH: f32 = 360.;
    /// Width of a field's label column: a dialog's, an inspector's properties, storage's facts.
    pub const FIELD_LABEL: f32 = 112.;
    /// A label column on a page, which reads at a document's width.
    pub const FIELD_LABEL_WIDE: f32 = 160.;
    /// The gap around and between glass sheets.
    pub const SHEET_GAP: f32 = 8.;
    /// The hit area of a divider that resizes a dock or a split, centred on its line.
    pub const RESIZE_HANDLE: f32 = 6.;
    /// The column of an empty state's actions.
    pub const STATE_ACTIONS: f32 = 240.;
    /// A disclosure chevron (`IconSize::XSmall`).
    pub const DISCLOSURE: f32 = 12.;
    /// A status or type dot.
    pub const DOT: f32 = 7.;
    /// A tag, a count badge, a write target: the small chips inside rows and headers. Also a
    /// switch's height.
    pub const TAG: f32 = 18.;
    /// A switch's width, and its knob, inside a [`TAG`]-high track.
    pub const SWITCH_WIDTH: f32 = 32.;
    pub const SWITCH_KNOB: f32 = 14.;
    /// A line being dragged (a column's or a dock's new edge).
    pub const DRAG_LINE: f32 = 2.;
    /// A coloured line down an item's leading edge: a change card's kind, a focused tree row.
    pub const ACCENT_LINE: f32 = 2.;
    /// The tallest a preview (an image, a document's first page) shows before it opens.
    pub const PREVIEW_MAX_HEIGHT: f32 = 240.;
    /// A short label before a value ("From", "To").
    pub const SHORT_LABEL: f32 = 40.;
    /// The tallest a modal grows before its body scrolls.
    pub const MODAL_MAX_HEIGHT: f32 = 560.;
    /// A type mark's initial (the 16 px mark), in its own small size.
    pub const MARK_TEXT: f32 = 9.;
    /// A new window's size when none was remembered.
    pub const WINDOW_WIDTH: f32 = 1440.;
    pub const WINDOW_HEIGHT: f32 = 900.;
    /// The least a code editor shrinks to when results share its column.
    pub const EDITOR_MIN_HEIGHT: f32 = 120.;
    /// A small gauge or spinner beside small text, and its stroke.
    pub const GAUGE: f32 = 14.;
    pub const GAUGE_STROKE: f32 = 2.;
    /// A document's own navigation column (the settings' sections).
    pub const NAV: f32 = 200.;
    /// A stepper's value between its − and +: two controls wide.
    pub const STEPPER_VALUE: f32 = 2. * CONTROL;
    /// A wide field or an inline preview of a long value (a command line).
    pub const FIELD_WIDE: f32 = 320.;
    /// A narrow field in a bar (a filter's value).
    pub const FIELD_NARROW: f32 = 180.;
    /// A field in a bar or a row (a bulk edit's value, a setting's text).
    pub const FIELD: f32 = 220.;
    /// A popover's narrowest and widest: a field's offers, a cell's notice.
    pub const POPOVER_MIN: f32 = 220.;
    pub const POPOVER_MAX: f32 = 420.;
    /// The tallest a list in a popover or a dialog grows before it scrolls.
    pub const LIST_MAX_HEIGHT: f32 = 240.;
    /// The widest a reading column or a floating bar over one grows: settings, a page, the
    /// table's drafts bar.
    pub const READING_WIDTH: f32 = 720.;
    /// A hairline border's width. A border sits inside its box, so it is part of the gap
    /// between a surface's edge and its children's (`radius::within`, `radius::around`).
    pub const HAIRLINE: f32 = 1.;
    /// A modal's width: enough for a sentence and a form, never the window.
    pub const MODAL_WIDTH: f32 = 480.;
    /// A wide modal: two panes side by side (opening a graph).
    pub const MODAL_WIDE: f32 = 720.;
    /// The command palette's width.
    pub const PALETTE_WIDTH: f32 = 580.;
    /// A context menu's row.
    pub const MENU_ROW: f32 = 26.;
    /// The narrowest a context menu draws.
    pub const MENU_MIN_WIDTH: f32 = 200.;
    /// A short vertical rule between groups in a strip: an icon's height.
    pub const SHORT_RULE: f32 = ROW_ICON;
    /// A label-and-value field's row in a property grid.
    pub const FIELD_ROW: f32 = GUTTER;
    /// A checkbox's box.
    pub const CHECK: f32 = GAUGE;
    /// The tallest a callout's description grows before it scrolls.
    pub const NOTICE_MAX_HEIGHT: f32 = 128.;
}

/// How far something recedes when it is present but not in play.
pub mod opacity {
    /// A row a filter dims, a card for a side not written yet, a merged branch's lane: one
    /// value, so every quiet thing is equally quiet.
    pub const DIMMED: f32 = 0.5;
    /// What lies away from the canvas's focus (its chips and their lines' labels): fainter than
    /// a dimmed row, since a crowded canvas has to let the near part stand out.
    pub const FAR: f32 = 0.3;
}

/// Whether the window is drawn in the glass style (setting `studio.window_style`).
pub fn is_glass(cx: &App) -> bool {
    cx.try_global::<StudioSettings>()
        .is_some_and(|settings| settings.studio.window_style == WindowStyle::Glass)
}

/// Surface colours by role.
pub mod surface {
    use super::*;

    /// A glass surface's fill at `alpha`: the light style's warm white, or in a dark theme the
    /// theme's own `base` surface, so the theme's text keeps its contrast on it.
    pub fn glass(base: Hsla, alpha: f32, cx: &App) -> Hsla {
        glass_fill(cx.theme().appearance.is_light(), base, alpha)
    }

    pub(crate) fn glass_fill(light: bool, base: Hsla, alpha: f32) -> Hsla {
        if light {
            Hsla::from(gpui::rgb(0xfcfbfc)).opacity(alpha)
        } else {
            base.opacity(alpha)
        }
    }

    /// How much ink each state laid over a glass sheet takes ([`ink`]).
    pub(crate) const RULE_INK: f32 = 0.14;
    pub(crate) const WELL_INK: f32 = 0.015;
    pub(crate) const SELECTED_INK: f32 = 0.05;
    pub(crate) const HOVER_INK: f32 = 0.025;
    pub(crate) const TAB_INK: f32 = 0.025;

    /// The ink a state is laid over a sheet in ([`rule`], [`well`], [`selected_chrome`],
    /// [`hover`], [`tab_inactive`]), at `alpha`: black on the light style's warm white; white in
    /// a dark theme, whose sheet is the theme's own dark surface ([`glass`]), where black would
    /// not show.
    pub(crate) fn ink(light: bool, alpha: f32) -> Hsla {
        if light {
            gpui::black().opacity(alpha)
        } else {
            gpui::white().opacity(alpha)
        }
    }

    fn ink_in(alpha: f32, cx: &App) -> Hsla {
        ink(cx.theme().appearance.is_light(), alpha)
    }

    /// The rule between regions and rows: the [`divider`] when flat; on glass 14% ink, which
    /// a row's or a section's dotted rule (`super::dotted`) lands at about half a solid line's
    /// weight. A region's edge (a toolbar's, a strip's, a pane's) stays solid in both styles.
    pub fn rule(cx: &App) -> Hsla {
        if is_glass(cx) {
            ink_in(RULE_INK, cx)
        } else {
            divider(cx)
        }
    }

    /// The hairline around a surface (a button, a card, a field, a menu), drawn as its 1 px
    /// border: interfaces.dev's `--shadow-border` ring, `#0000000f` in a light theme and
    /// `#ffffff14` in a dark one. Its hovered and pressed steps are [`outline_hover`] and
    /// [`outline_pressed`]; the drops that go with it are [`super::elevation::custom`].
    pub fn outline(cx: &App) -> Hsla {
        super::elevation::layers(super::elevation::Lift::Rest, cx)[0].color
    }

    /// The hovered [`outline`] (`--shadow-custom-hover`'s ring): `#00000014` / `#ffffff21`.
    pub fn outline_hover(cx: &App) -> Hsla {
        super::elevation::layers(super::elevation::Lift::Hover, cx)[0].color
    }

    /// The pressed or focused [`outline`] (`--shadow-custom-active`'s ring): `#0000001f` /
    /// `#ffffff33`.
    pub fn outline_pressed(cx: &App) -> Hsla {
        super::elevation::layers(super::elevation::Lift::Pressed, cx)[0].color
    }

    /// A line that divides (rows in a list, a toolbar's edge, a checkbox's or an outlined
    /// chip's edge): interfaces.dev's `--color-gray-400`, `#e8e8e8` on a white page, as 9 %
    /// black so it holds on any fill; in a dark theme 8 % white (its `#1f1f1f` on `#0b0b0b`).
    pub fn divider(cx: &App) -> Hsla {
        if cx.theme().appearance.is_light() {
            gpui::black().opacity(23. / 255.)
        } else {
            gpui::white().opacity(20. / 255.)
        }
    }

    /// The canvas's ground, under its chips: interfaces.dev's `--color-gray-background`
    /// (`#fcfcfc`), so a white chip and its hairline read as they do there; in a dark theme
    /// the document's own fill, under lighter chips.
    pub fn canvas(cx: &App) -> Hsla {
        if cx.theme().appearance.is_light() {
            document(cx).blend(gpui::black().opacity(3. / 255.))
        } else {
            document(cx)
        }
    }

    /// An outlined control's fill when hovered: interfaces.dev's `hover:bg-gray-100`, `#fcfcfc`
    /// over white, and in a dark theme its `#111` over `#0b0b0b`.
    pub fn control_hover(fill: Hsla, cx: &App) -> Hsla {
        if cx.theme().appearance.is_light() {
            fill.blend(gpui::black().opacity(3. / 255.))
        } else {
            fill.blend(gpui::white().opacity(6. / 255.))
        }
    }

    /// A recessed area inside a container (an empty list, a code block on glass): a faint ink
    /// rather than a fill, so the glass still shows through; a darkening in a light theme, a
    /// lightening in a dark one.
    pub fn well(cx: &App) -> Hsla {
        ink_in(WELL_INK, cx)
    }

    /// A card's hairline, and the rule inside it over its footer: the [`outline`].
    pub fn card_border(cx: &App) -> Hsla {
        outline(cx)
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
            ink_in(SELECTED_INK, cx)
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
            ink_in(HOVER_INK, cx)
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

    /// A modal's or a menu's fill: the elevated surface, opaque in both styles. Tower's glass
    /// blurs what is behind it and GPUI cannot, so even a 97 % fill lets the text behind show
    /// through as ghosts.
    pub fn floating(cx: &App) -> Hsla {
        cx.theme().colors().elevated_surface_background
    }

    /// The hairline of a floating surface (a modal, a menu, a chip on the canvas): the
    /// [`outline`], so every surface has the one hairline.
    pub fn floating_border(cx: &App) -> Hsla {
        outline(cx)
    }

    /// The light veil behind the command palette: the page keeps its colour behind it.
    pub fn veil(cx: &App) -> Hsla {
        if cx.theme().appearance.is_light() {
            gpui::black().opacity(0.08)
        } else {
            gpui::black().opacity(0.36)
        }
    }

    /// The dim behind a dialog. Tower's also blurs; GPUI cannot, so it is a little lighter.
    pub fn backdrop(cx: &App) -> Hsla {
        if cx.theme().appearance.is_light() {
            gpui::black().opacity(0.24)
        } else {
            gpui::black().opacity(0.48)
        }
    }

    /// A hovered control's fill and the highlighted menu row's: a faint ink.
    pub fn highlight(cx: &App) -> Hsla {
        cx.theme().colors().text.opacity(0.045)
    }

    /// The pressed fill: a selected tab, a toggled button, a control while it is pressed.
    pub fn pressed(cx: &App) -> Hsla {
        cx.theme().colors().text.opacity(0.06)
    }

    /// The glass window's base, under [`super::wash`].
    pub fn page(cx: &App) -> Hsla {
        cx.theme().colors().editor_background
    }

    /// The window's own fill when flat, behind the columns.
    pub fn window(cx: &App) -> Hsla {
        cx.theme().colors().background
    }

    /// The active tab: clear on glass, the document's fill when flat, so it joins the document.
    pub fn tab_active(cx: &App) -> Hsla {
        if is_glass(cx) {
            gpui::transparent_black()
        } else {
            cx.theme().colors().editor_background
        }
    }

    /// An inactive tab: barely shaded on glass, the tab strip's tint when flat.
    pub fn tab_inactive(cx: &App) -> Hsla {
        if is_glass(cx) {
            ink_in(TAB_INK, cx)
        } else {
            cx.theme().colors().tab_bar_background.opacity(0.35)
        }
    }

    /// A pane's header strip: clear on glass, the sunken tint when flat.
    pub fn header_strip(cx: &App) -> Hsla {
        if is_glass(cx) {
            gpui::transparent_black()
        } else {
            sunken(cx)
        }
    }

    /// The document's own fill: a table, a query, a page, a solid card or field on glass.
    pub fn document(cx: &App) -> Hsla {
        cx.theme().colors().editor_background
    }

    /// The fill of the window's chrome when flat: the docks and their panels.
    pub fn chrome(cx: &App) -> Hsla {
        cx.theme().colors().panel_background
    }

    /// The status bar's fill.
    pub fn status_bar(cx: &App) -> Hsla {
        cx.theme().colors().status_bar_background
    }

    /// Where a dragged tab or column will land.
    pub fn drop_target(cx: &App) -> Hsla {
        cx.theme().colors().drop_target_background
    }

    /// The edge of a checkbox, an outlined chip or a composer control: the [`divider`], as
    /// interfaces.dev's checkbox (`border-gray-400`). A field or a button is a surface and has
    /// the [`outline`] instead.
    pub fn control_border(cx: &App) -> Hsla {
        divider(cx)
    }

    /// The focused control's ring, and a line being dragged (a column's new width).
    pub fn focus(cx: &App) -> Hsla {
        cx.theme().colors().border_focused
    }

    /// The accent: a selection on the canvas, the commit a view stands on.
    pub fn accent(cx: &App) -> Hsla {
        cx.theme().colors().text_accent
    }

    /// A notice's fill and hairline by severity (a banner, a callout): the theme's status
    /// colours, faint, as Zed's banner draws them.
    pub fn notice(severity: ui::Severity, cx: &App) -> (Hsla, Hsla) {
        let status = cx.theme().status();
        match severity {
            ui::Severity::Info => (
                status.info_background.opacity(0.5),
                cx.theme().colors().border.opacity(0.5),
            ),
            ui::Severity::Success => (status.success.opacity(0.1), status.success.opacity(0.2)),
            ui::Severity::Warning => (
                status.warning_background.opacity(0.5),
                status.warning_border.opacity(0.4),
            ),
            ui::Severity::Error => (status.error.opacity(0.1), status.error.opacity(0.2)),
        }
    }

    /// The canvas's marquee while it is dragged: the accent, faint inside, firmer at its edge.
    pub fn marquee(cx: &App) -> Hsla {
        accent(cx).opacity(0.08)
    }

    pub fn marquee_border(cx: &App) -> Hsla {
        accent(cx).opacity(0.6)
    }

    /// A span marked in an editor's text (the query an explain step came from): the accent,
    /// faint enough to read the text through it.
    pub fn text_highlight(cx: &App) -> Hsla {
        accent(cx).opacity(0.16)
    }

    /// Rows picked beside the cursor's row: the selected row's fill, lighter, so the cursor
    /// still stands out among them.
    pub fn picked(cx: &App) -> Hsla {
        selected_document(cx).opacity(0.6)
    }

    /// Where you are on the canvas (the selected or marked chip): a warm ring, apart from the
    /// accent a selection's marquee takes.
    pub fn here(_cx: &App) -> Hsla {
        gpui::rgb(0xf2b48f).into()
    }

    /// The halo around [`here`]'s ring.
    pub fn here_halo(cx: &App) -> Hsla {
        here(cx).opacity(0.2)
    }

    /// A relationship's label on the canvas: a chip's fill, a little clear so the line under it
    /// still reads.
    pub fn canvas_label(cx: &App) -> Hsla {
        solid(cx).opacity(0.95)
    }

    /// A closed hub drawn from afar as a dot on its line: the ink at a quarter.
    pub fn canvas_hub(cx: &App) -> Hsla {
        cx.theme().colors().text.opacity(0.25)
    }

    /// The canvas's dot grid: the ink, barely there.
    pub fn grid_dot(cx: &App) -> Hsla {
        cx.theme().colors().text.opacity(0.07)
    }

    /// A bar that asks before something irreversible (a write the agent prepared): the theme's
    /// warning fill and hairline.
    pub fn warning(cx: &App) -> Hsla {
        cx.theme().status().warning_background
    }

    pub fn warning_border(cx: &App) -> Hsla {
        cx.theme().status().warning_border
    }

    /// The hairline of a field or chip that refuses its value.
    pub fn error_border(cx: &App) -> Hsla {
        cx.theme().status().error_border
    }

    /// A page of a document (a PDF's page): paper white in either theme.
    pub fn paper(_cx: &App) -> Hsla {
        gpui::white()
    }

    /// Text on a solid tint (a type mark's initial, a solid button's label): white.
    pub fn on_tint(_cx: &App) -> Hsla {
        gpui::white()
    }

    /// A solid item floating over a document or the canvas (a bar over a table, a chip on the
    /// canvas): white in the light theme, the elevated surface in a dark one.
    pub fn solid(cx: &App) -> Hsla {
        if cx.theme().appearance.is_light() {
            gpui::white()
        } else {
            cx.theme().colors().elevated_surface_background
        }
    }
}

/// Elevation as layered transparent shadows (better-ui: shadows for elevation, borders for
/// structure).
///
/// Every value here is a CSS `box-shadow` layer as a browser draws it. GPUI takes a shadow's
/// `blur_radius` as the Gaussian's sigma, where CSS's blur is twice the sigma, so a layer's
/// blur is halved on the way in ([`Layer::shadow`]); copied as written, it would draw twice as
/// soft.
pub mod elevation {
    use super::*;

    /// One CSS `box-shadow` layer, in the reference's own terms: `blur` is CSS's, and `spread`
    /// grows (or shrinks) the box it is cast from.
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub struct Layer {
        pub color: Hsla,
        pub y: f32,
        pub blur: f32,
        pub spread: f32,
    }

    impl Layer {
        const fn black(alpha: f32, y: f32, blur: f32, spread: f32) -> Self {
            Self {
                color: Hsla {
                    h: 0.,
                    s: 0.,
                    l: 0.,
                    a: alpha,
                },
                y,
                blur,
                spread,
            }
        }

        const fn white(alpha: f32, y: f32, blur: f32, spread: f32) -> Self {
            Self {
                color: Hsla {
                    h: 0.,
                    s: 0.,
                    l: 1.,
                    a: alpha,
                },
                y,
                blur,
                spread,
            }
        }

        /// The layer as GPUI draws it under the same box: CSS's blur is twice GPUI's sigma.
        pub fn shadow(self) -> BoxShadow {
            BoxShadow {
                color: self.color,
                offset: point(px(0.), px(self.y)),
                blur_radius: px(self.blur / 2.),
                spread_radius: px(self.spread),
                inset: false,
            }
        }

        /// The corner a browser gives this layer on a box whose corner is `radius`
        /// (CSS Backgrounds 3, "spread distance"): a spread adds to the corner, scaled down
        /// when the corner is smaller than the spread, and a negative one takes from it.
        pub fn corner(self, radius: f32) -> f32 {
            let spread = self.spread;
            if spread < 0. {
                (radius + spread).max(0.)
            } else if radius <= 0. || spread == 0. {
                radius
            } else if radius >= spread {
                radius + spread
            } else {
                let ratio = radius / spread;
                radius + spread * (1. + (ratio - 1.).powi(3))
            }
        }
    }

    /// How engaged a surface is: at rest, under the pointer, pressed or focused.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Lift {
        Rest,
        Hover,
        Pressed,
    }

    /// interfaces.dev's `--shadow-custom`, its `-hover` and its `-active`, the one shadow of
    /// every button, chip, pill and card there: a 1 px ring, then two soft drops in a light
    /// theme; the ring alone in a dark one. The ring comes first.
    const LIGHT: [[Layer; 3]; 3] = [
        [
            Layer::black(15. / 255., 0., 0., 1.),
            Layer::black(15. / 255., 1., 2., -1.),
            Layer::black(10. / 255., 2., 4., 0.),
        ],
        [
            Layer::black(20. / 255., 0., 0., 1.),
            Layer::black(20. / 255., 1., 2., -1.),
            Layer::black(15. / 255., 2., 4., 0.),
        ],
        [
            Layer::black(31. / 255., 0., 0., 1.),
            Layer::black(26. / 255., 1., 2., -1.),
            Layer::black(20. / 255., 2., 4., 0.),
        ],
    ];
    const DARK: [Layer; 3] = [
        Layer::white(20. / 255., 0., 0., 1.),
        Layer::white(33. / 255., 0., 0., 1.),
        Layer::white(51. / 255., 0., 0., 1.),
    ];

    /// The layers of [`LIGHT`] or [`DARK`] at `lift`, the ring first.
    pub fn layers(lift: Lift, cx: &App) -> Vec<Layer> {
        let step = match lift {
            Lift::Rest => 0,
            Lift::Hover => 1,
            Lift::Pressed => 2,
        };
        if dark(cx) {
            vec![DARK[step]]
        } else {
            LIGHT[step].to_vec()
        }
    }

    /// [`layers`] at `lift` as an element's shadows, the ring first and outside the box, as
    /// CSS draws them: the drops darken the ring where they cross it, which a ring drawn as the
    /// element's border would hide under its fill. The element's own border is clear
    /// ([`super::lifted`]). GPUI keeps the element's corner for every layer where CSS grows it
    /// with the spread, so the ring is a little heavier at a corner.
    pub fn lift(lift: Lift, cx: &App) -> Vec<BoxShadow> {
        layers(lift, cx).into_iter().map(Layer::shadow).collect()
    }

    /// interfaces.dev's `--shadow-custom` at rest: a card, a field, a pill, a bar, an outlined
    /// button ([`lift`]).
    pub fn custom(cx: &App) -> Vec<BoxShadow> {
        lift(Lift::Rest, cx)
    }

    /// Paints `layers` under a shape its painter draws itself (a chip on the canvas) as a
    /// browser draws a CSS `box-shadow` on a box of `bounds` and `radius`: the ring outside
    /// the shape, and each layer's corner grown or shrunk with its spread.
    pub fn paint_layers(
        bounds: Bounds<Pixels>,
        radius: f32,
        layers: &[Layer],
        window: &mut Window,
    ) {
        for layer in layers {
            window.paint_drop_shadows(
                bounds,
                Corners::all(px(layer.corner(radius))),
                &[layer.shadow()],
            );
        }
    }

    /// A solid button's shadow (interfaces.dev's primary): a faint white haze inside
    /// (`inset 0 0 1px 1px`), a 1.5 px drop at 10 % black, and a 1 px ring outside in the
    /// button's own `fill`.
    pub fn filled(fill: Hsla) -> Vec<BoxShadow> {
        vec![
            BoxShadow {
                color: gpui::white().opacity(0.06),
                offset: point(px(0.), px(0.)),
                blur_radius: px(0.5),
                spread_radius: px(size::HAIRLINE),
                inset: true,
            },
            Layer::black(0.1, 1.5, 2., 0.).shadow(),
            Layer {
                color: fill,
                y: 0.,
                blur: 0.,
                spread: size::HAIRLINE,
            }
            .shadow(),
        ]
    }

    fn shadow(alpha: f32, y: f32, blur: f32, spread: f32) -> BoxShadow {
        Layer::black(alpha, y, blur, spread).shadow()
    }

    fn rim(alpha: f32) -> BoxShadow {
        BoxShadow {
            color: gpui::white().opacity(alpha),
            offset: point(px(0.), px(0.)),
            blur_radius: px(0.),
            spread_radius: px(size::HAIRLINE),
            inset: true,
        }
    }

    fn dark(cx: &App) -> bool {
        !cx.theme().appearance.is_light()
    }

    // Tower's values, light and dark (`styles/glass.css`, `styles/theme.css`, `button.tsx`).

    /// [`custom`]'s drops alone, for an element that draws its own hairline as its border (a
    /// document page with its rule, the palette's results card): the box includes that line,
    /// so each drop's spread is one less than CSS's.
    pub fn raised(cx: &App) -> Vec<BoxShadow> {
        layers(Lift::Rest, cx)
            .into_iter()
            .skip(1)
            .map(|layer| {
                Layer {
                    spread: layer.spread - size::HAIRLINE,
                    ..layer
                }
                .shadow()
            })
            .collect()
    }

    /// An outlined control (an outline button, a field): [`custom`].
    pub fn control(cx: &App) -> Vec<BoxShadow> {
        custom(cx)
    }

    /// A glass sheet, and everything that floats over the page (a menu, a popover, a modal):
    /// an inner rim, a contact shadow and a soft one offset downward.
    pub fn sheet(cx: &App) -> Vec<BoxShadow> {
        if dark(cx) {
            vec![
                rim(0.06),
                shadow(0.3, 1., 2., 0.),
                shadow(0.6, 12., 28., -9.),
            ]
        } else {
            vec![
                rim(0.7),
                shadow(0.04, 1., 2., 0.),
                shadow(0.11, 12., 28., -9.),
            ]
        }
    }

    /// Small floating glass controls (bars, chips over a canvas): a shorter, tighter shadow.
    pub fn float(cx: &App) -> Vec<BoxShadow> {
        if dark(cx) {
            vec![
                rim(0.06),
                shadow(0.3, 1., 2., 0.),
                shadow(0.5, 6., 14., -6.),
            ]
        } else {
            vec![
                rim(0.7),
                shadow(0.05, 1., 2., 0.),
                shadow(0.12, 6., 14., -6.),
            ]
        }
    }

    /// A modal, a menu or a popover: the glass sheet's shadow, as Tower gives every popup.
    pub fn floating(cx: &App) -> Vec<BoxShadow> {
        sheet(cx)
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
    icon_slot(
        div()
            .size(px(size::DOT))
            .rounded_full()
            .bg(studio_theme::type_color(type_name)),
    )
}

pub fn status_dot(status: Status) -> Div {
    div()
        .size(px(size::DOT))
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

/// Icon roles, as the text roles: a quiet icon beside a title or a label, and a smaller one
/// among metadata or inside a dense row. An icon in a button takes the button's own size.
pub mod icon {
    use super::*;

    /// An icon that leads a toolbar's title, a section or a menu row: small and muted.
    pub fn lead(name: IconName) -> Icon {
        Icon::new(name).size(IconSize::Small).color(Color::Muted)
    }

    /// An icon among metadata or inside a dense row (a relationship's arrow, a hint's mark):
    /// extra small and muted.
    pub fn meta(name: IconName) -> Icon {
        Icon::new(name).size(IconSize::XSmall).color(Color::Muted)
    }
}

/// Text roles. Body text is `Small`; metadata and labels `XSmall`; one weight change per row.
pub mod text {
    use super::*;

    /// Font weights by role. Inter's heavier cuts read heavy at Studio's sizes, so there are
    /// two: the body's, for everything that is read (rows, buttons, segments, a key column),
    /// and the heading's, for what names a surface (a toolbar's, a page's or a dialog's title,
    /// a tree's root, a card's subject).
    pub mod weight {
        use gpui::FontWeight;

        pub const BODY: FontWeight = FontWeight::NORMAL;
        pub const HEADING: FontWeight = FontWeight::MEDIUM;
    }

    /// A toolbar's or a card's title: the default size, a step above body text, and the
    /// heading weight.
    pub fn title(text: impl Into<SharedString>) -> Label {
        Label::new(text.into()).weight(weight::HEADING)
    }

    /// A page's section heading (a settings page's) or a dialog's title: a step larger than a
    /// title.
    pub fn heading(text: impl Into<SharedString>) -> Label {
        Label::new(text.into())
            .size(LabelSize::Large)
            .weight(weight::HEADING)
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

    /// Text at the default size in ink: a menu row's label, a view's summary sentence.
    pub fn plain(text: impl Into<SharedString>) -> Label {
        Label::new(text.into())
    }

    /// The one line of an empty or waiting state ("No tab open", "Reading…", "Choose an
    /// agent"): the default size, muted.
    pub fn state(text: impl Into<SharedString>) -> Label {
        Label::new(text.into()).color(Color::Muted)
    }

    /// A refusal or a failure in a sentence: the body size in the error colour.
    pub fn error(text: impl Into<SharedString>) -> Label {
        Label::new(text.into())
            .size(LabelSize::Small)
            .color(Color::Error)
    }

    /// A refusal noted under a field or beside a row: the metadata size in the error colour.
    pub fn error_note(text: impl Into<SharedString>) -> Label {
        Label::new(text.into())
            .size(LabelSize::XSmall)
            .color(Color::Error)
    }

    /// A page's title, in px at Zed's 16 px rem: the one large heading a node's page carries.
    pub const PAGE_TITLE: f32 = 22.;
    /// A key binding's glyphs in a hint, in px at Zed's 16 px rem: the meta size.
    pub const KEY_HINT: f32 = 11.;

    /// A page's title size.
    pub fn page_title_size() -> LabelSize {
        LabelSize::Custom(rems_from_px(PAGE_TITLE))
    }

    /// The one large title a node's page carries.
    pub fn page_title(text: impl Into<SharedString>) -> Label {
        Label::new(text.into())
            .size(page_title_size())
            .weight(weight::HEADING)
    }

    /// The default size, for text that is not a `Label` (a summary set in a `div`): what
    /// [`plain`], [`title`] and [`state`] draw at.
    pub fn plain_size(cx: &App) -> Rems {
        ui::TextSize::Default.rems(cx)
    }

    /// The body size, for text that is not a `Label` (a markdown value, a grid cell, an
    /// editor's font): what [`body`], [`muted`] and [`error`] draw at.
    pub fn body_size(cx: &App) -> Rems {
        ui::TextSize::Small.rems(cx)
    }

    /// The metadata size, for text that is not a `Label`: what [`meta`] and [`code`] draw at.
    pub fn meta_size(cx: &App) -> Rems {
        ui::TextSize::XSmall.rems(cx)
    }

    /// A labelled control's icon beside its label: 80 % of the label's ink, as a button's.
    pub fn icon_ink(cx: &App) -> Color {
        Color::Custom(cx.theme().colors().text.opacity(0.8))
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
        .child(icon::meta(if open {
            IconName::ChevronDown
        } else {
            IconName::ChevronRight
        }))
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

    /// What leads the label (a type mark, a status dot), centered in the row's icon slot.
    pub fn start(mut self, start: impl IntoElement) -> Self {
        self.start = Some(start.into_any_element());
        self
    }

    /// A muted icon leading the label, at the size every sidebar list uses.
    pub fn icon(self, icon: IconName) -> Self {
        self.start(icon::lead(icon))
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
            .rounded(px(radii(cx).sm))
            .cursor_pointer()
            .when(self.selected, |row| {
                row.bg(selected_bg)
                    .when(glass, |row| row.shadow(elevation::raised(cx)))
            })
            .when(!self.selected, |row| {
                row.hover(move |style| style.bg(hover_bg))
            })
            .when_some(self.tooltip, |row, tooltip| row.tooltip(tooltip))
            .children(self.start.map(icon_slot))
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

/// A row's icon slot ([`size::ROW_ICON`]), with what leads the label centered in it: an icon,
/// a type's dot or mark.
pub fn icon_slot(start: impl IntoElement) -> Div {
    div()
        .size(px(size::ROW_ICON))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .child(start)
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
        .child(icon_slot(
            Icon::new(icon).size(IconSize::Small).color(Color::Disabled),
        ))
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
        .child(div().flex_1().min_w_0().child(text::muted(label)))
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

/// A button in a document's toolbar: Zed's 22 px size (Tower's `xs`), small type and icons, so
/// the toolbar reads as the document's chrome rather than as its content.
pub fn toolbar_button(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Button {
    button(id, label).size(ButtonSize::Default)
}

/// An icon button in a document's toolbar, beside [`toolbar_button`]s.
pub fn toolbar_icon_button(id: impl Into<ElementId>, icon: IconName) -> IconButton {
    icon_button(id, icon).size(ButtonSize::Default)
}

/// A switch: a pill track and a round knob. On, the track is the primary button's ink; off,
/// the [`surface::divider`]'s grey; the knob is the solid surface with interfaces.dev's shadow
/// in both, so it reads on either. Studio draws it rather than Zed's `Switch`, whose knob takes
/// the theme's text colour, the ink of an on track, so an on switch showed no knob.
pub fn switch(id: impl Into<ElementId>, state: ui::ToggleState, _cx: &App) -> Switch {
    Switch {
        id: id.into(),
        on: state == ui::ToggleState::Selected,
        on_click: None,
    }
}

/// What a [`Switch`] calls with the state a click turns it to.
type SwitchHandler = Box<dyn Fn(&ui::ToggleState, &mut Window, &mut App) + 'static>;

/// See [`switch`].
#[derive(IntoElement)]
pub struct Switch {
    id: ElementId,
    on: bool,
    on_click: Option<SwitchHandler>,
}

impl Switch {
    /// Called with the state a click turns the switch to.
    pub fn on_click(
        mut self,
        handler: impl Fn(&ui::ToggleState, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Box::new(handler));
        self
    }
}

impl RenderOnce for Switch {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let ink = cx.theme().colors().text;
        let (track, hover) = if self.on {
            (ink, ink.opacity(0.9))
        } else {
            let divider = surface::divider(cx);
            (divider, divider.blend(surface::highlight(cx)))
        };
        let next = if self.on {
            ui::ToggleState::Unselected
        } else {
            ui::ToggleState::Selected
        };
        let inset = (size::TAG - size::SWITCH_KNOB) / 2.;
        h_flex()
            .id(self.id)
            .role(gpui::Role::Switch)
            .aria_toggled(if self.on {
                gpui::Toggled::True
            } else {
                gpui::Toggled::False
            })
            .flex_none()
            .w(px(size::SWITCH_WIDTH))
            .h(px(size::TAG))
            .px(px(inset))
            .when(self.on, |track| track.justify_end())
            .rounded_full()
            .bg(track)
            .hover(move |style| style.bg(hover))
            .cursor_pointer()
            .when_some(self.on_click, |track, on_click| {
                track.on_click(move |_, window, cx| on_click(&next, window, cx))
            })
            .child(
                div()
                    .size(px(size::SWITCH_KNOB))
                    .rounded_full()
                    .bg(surface::solid(cx))
                    .shadow(elevation::custom(cx)),
            )
    }
}

/// A toast's surface: on glass a small floating glass surface, like the canvas's bars; flat,
/// the floating fill, hairline and shadow.
pub fn toast_surface<E: Styled>(element: E, cx: &App) -> E {
    if is_glass(cx) {
        element
            .bg(surface::glass(
                cx.theme().colors().elevated_surface_background,
                0.9,
                cx,
            ))
            .shadow(elevation::float(cx))
    } else {
        element
            .bg(surface::floating(cx))
            .border_1()
            .border_color(surface::floating_border(cx))
            .shadow(elevation::floating(cx))
    }
}

/// What follows the pointer while a tab or a column is dragged: a small solid chip.
pub fn drag_preview(cx: &App) -> Div {
    h_flex()
        .px(px(space::MD))
        .py(px(space::XS))
        .gap(px(space::MD))
        .items_center()
        .rounded(px(radii(cx).sm))
        .bg(surface::floating(cx))
        .border_1()
        .border_color(surface::floating_border(cx))
        .shadow(elevation::float(cx))
}

/// A toolbar's buttons, set close: a ghost button's own padding is the space between them.
pub fn toolbar_actions() -> Div {
    h_flex().flex_none().items_center()
}

/// A hairline that fills the row it sits in, as either side of a centred "or".
pub fn rule_h(cx: &App) -> Div {
    div().flex_1().h(px(size::HAIRLINE)).bg(surface::rule(cx))
}

/// The divider between groups of the status bar, as Zed's: a short vertical line in the border
/// colour, which a hairline's lighter colour would lose against the bar.
pub fn divider_v(cx: &App) -> Div {
    div()
        .w(px(size::HAIRLINE))
        .h(px(size::SHORT_RULE))
        .flex_none()
        .bg(cx.theme().colors().border)
}

/// Metadata as an icon and a short value (a count, a commit, a branch), instead of a sentence.
pub fn meta(icon: IconName, value: impl Into<SharedString>) -> Div {
    h_flex()
        .gap(px(space::XS))
        .items_center()
        .flex_none()
        .child(icon::meta(icon))
        .child(text::meta(value).single_line())
}

/// [`meta`] with the value in the buffer font, for commit ids and keys.
pub fn meta_code(icon: IconName, value: impl Into<SharedString>, cx: &App) -> Div {
    h_flex()
        .gap(px(space::XS))
        .items_center()
        .flex_none()
        .child(icon::meta(icon))
        .child(text::code(value, cx).color(Color::Muted).single_line())
}

/// A button in a toolbar, strip, panel or dialog: 28 px tall with a 6 px inset and a small
/// label, so its hover and pressed fills have room around the label without padding it out
/// (`crate::button`). A button inside a row or a card is a [`row_button`].
pub fn button(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Button {
    Button::new(id, label)
}

/// Work under way that the person waits on (a search, a run, rows being read): a small turning
/// circle, muted, beside or in place of what it will fill.
pub fn spinner() -> impl IntoElement {
    Icon::new(IconName::ArrowCircle)
        .size(IconSize::XSmall)
        .color(Color::Muted)
        .with_rotate_animation(2)
}

/// [`button`] inside a [`float_bar`]: Zed's default size, `size::BAR_BUTTON` tall.
pub fn bar_button(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Button {
    button(id, label)
        .size(ButtonSize::Default)
        .shape(Shape::Rounded)
}

/// [`icon_button`] inside a [`float_bar`].
pub fn bar_icon_button(id: impl Into<ElementId>, icon: IconName) -> IconButton {
    icon_button(id, icon).size(ButtonSize::Default)
}

/// [`button`] for an icon alone: a 28 px square.
pub fn icon_button(id: impl Into<ElementId>, icon: IconName) -> IconButton {
    IconButton::new(id, icon).size(ButtonSize::Medium)
}

/// A labelled action inside a row or a card ("Show all", "Load more", "Discard"):
/// `size::ROW_BUTTON` tall, so it sits in the row as a tag does.
pub fn row_button(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Button {
    button(id, label).size(ButtonSize::Compact)
}

/// An icon action inside a row, a card's line or beside a value (pin, copy, reset, refresh,
/// remove, open): a `size::ROW_BUTTON` square with the small icon, muted until hovered.
pub fn row_icon_button(id: impl Into<ElementId>, icon: IconName) -> IconButton {
    IconButton::new(id, icon).size(ButtonSize::Compact)
}

/// An icon inside a tab beside its title (pin, close): no square of its own beyond the icon,
/// so the tab keeps its height and the title its room.
pub fn inline_icon_button(id: impl Into<ElementId>, icon: IconName) -> IconButton {
    IconButton::new(id, icon).size(ButtonSize::None)
}

/// A sheet's close (a dialog's ×): an outlined square at a strip's size with a row's icon, so
/// it holds its place beside the title without weighing as much as it.
pub fn sheet_close_button(id: impl Into<ElementId>) -> IconButton {
    IconButton::new(id, IconName::Close)
        .variant(Variant::Outline)
        .size(ButtonSize::Default)
        .icon_size(IconSize::XSmall)
}

/// A text field's frame: a hairline box at a control's height, so a field lines up with the
/// buttons beside it, around an editor and an icon or a counter. `invalid` draws the error
/// border. Every single-line field in Studio is one, but for a filter among a sidebar's rows
/// ([`sidebar_search`]).
pub fn field_frame(invalid: bool, cx: &App) -> Div {
    h_flex()
        .h(px(size::CONTROL))
        // The border is part of the inset.
        .px(px(space::CONTROL_INSET - 1.))
        .gap(px(space::SM))
        .items_center()
        .rounded(px(radii(cx).sm))
        .border_1()
        .bg(surface::document(cx))
        // interfaces.dev's field: the surface's ring and drops; an invalid one has its red
        // line instead of the ring.
        .map(|field| {
            if invalid {
                field
                    .border_color(surface::error_border(cx))
                    .shadow(elevation::raised(cx))
            } else {
                field
                    .border_color(gpui::transparent_black())
                    .shadow(elevation::control(cx))
            }
        })
}

/// A field's editor, filling what its [`field_frame`] leaves.
pub fn field_input(editor: impl IntoElement) -> Div {
    div().flex_1().min_w_0().child(editor)
}

/// A search field: a [`field_frame`] led by a magnifying glass.
pub fn search_field(editor: impl IntoElement, cx: &App) -> Div {
    field_frame(false, cx)
        .child(
            Icon::new(IconName::MagnifyingGlass)
                .size(IconSize::XSmall)
                .color(Color::Muted),
        )
        .child(field_input(editor))
}

/// A filter at the top of a sidebar's list: a sunken row as tall as the rows under it, its
/// text on their edge, as their titles are, with no border, since it is part of the list.
pub fn sidebar_search(editor: impl IntoElement, cx: &App) -> Div {
    h_flex()
        .h(px(size::SIDEBAR_ROW))
        .mx(px(space::SM))
        .mb(px(space::XS))
        .px(px(space::SM))
        .items_center()
        .rounded(px(radii(cx).sm))
        .bg(surface::sunken(cx))
        .child(field_input(editor))
}

/// The text of a single-line field that sits among `Small` labels (a filter value, a search
/// box): an editor's text at the label's size rather than the body size.
pub fn field_text(cx: &App) -> gpui::TextStyleRefinement {
    gpui::TextStyleRefinement {
        font_size: Some(text::body_size(cx).into()),
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
    let divider = move || div().w(px(size::HAIRLINE)).h_full().flex_none().bg(rule);
    let segment = || {
        h_flex()
            .h_full()
            .px(px(space::MD))
            .gap(px(space::XS))
            .items_center()
            .flex_none()
    };
    h_flex()
        .id(id)
        .h(px(size::PILL))
        .flex_none()
        .rounded_full()
        .bg(surface::document(cx))
        .border_1()
        .border_color(gpui::transparent_black())
        .shadow(elevation::custom(cx))
        .overflow_hidden()
        .child(
            segment()
                .child(icon::meta(icon))
                .child(Label::new(field.into()).size(LabelSize::Small)),
        )
        .child(divider())
        .child(segment().child(text::muted(test).size(LabelSize::Small)))
        .when_some(value, |pill, value| {
            pill.child(divider()).child(
                segment().max_w(px(size::FIELD)).child(
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
                .pl(px(space::XS))
                // The pill's border is the rest of the gap.
                .pr(px(PILL_BUTTON_GAP - 1.))
                .flex()
                .items_center()
                .child(
                    pill_button("remove", IconName::Close, Color::Muted, cx)
                        .tooltip(Tooltip::text("Remove this filter"))
                        .on_click(on_remove),
                ),
        )
}

/// How far a [`pill_button`] sits inside its pill's edge, its border included: the pill's end
/// pads by this less the border.
pub const PILL_BUTTON_GAP: f32 = space::XS;

/// An icon button at a pill's end, [`PILL_BUTTON_GAP`] inside it: round, so its fill follows
/// the pill's round end (`radius::within(size::PILL / 2, gap)` is its own half height) where
/// Zed's 4 px button would sit square in it. Pressed, it acts as Zed's buttons do: the focus
/// stays where it was (a pill's field), and the click goes no further than it.
pub fn pill_button(
    id: impl Into<ElementId>,
    icon: IconName,
    color: Color,
    cx: &App,
) -> Stateful<Div> {
    let diameter = size::PILL - 2. * PILL_BUTTON_GAP;
    let hover = surface::highlight(cx);
    let active = surface::pressed(cx);
    div()
        .id(id.into())
        .size(px(diameter))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .cursor_pointer()
        .hover(move |style| style.bg(hover))
        .active(move |style| style.bg(active))
        .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
        .on_click(|_, _, cx| cx.stop_propagation())
        .child(Icon::new(icon).size(IconSize::XSmall).color(color))
}

/// A padded list of rounded rows in a form or a dialog (a field's offers, what a delete takes
/// with it): rows of radius `Radii::xs` on the document's fill inside a floating surface's
/// hairline, the list concentric around them.
pub fn row_list(cx: &App) -> Div {
    v_flex()
        .p(px(space::XS))
        .border_1()
        .rounded(px(radii(cx).row_list()))
        .bg(surface::document(cx))
        .border_color(surface::floating_border(cx))
}

/// A [`row_list`] floating over the page under a field (a filter's values): a menu's fill
/// and shadow.
pub fn popover_list(cx: &App) -> Div {
    row_list(cx)
        .bg(surface::floating(cx))
        .shadow(elevation::floating(cx))
}

/// A floating group of controls over a canvas: a glass bar 28 px tall holding
/// [`bar_button`]s, [`bar_icon_button`]s (22 px) and [`segment`]s, 3 px inside its edge and
/// rounded concentrically around them (`Radii::bar`). A medium button would not fit inside its
/// border.
pub fn float_bar(id: impl Into<ElementId>, cx: &App) -> Stateful<Div> {
    let gap = (size::CONTROL - size::BAR_BUTTON) / 2.;
    let colors = cx.theme().colors();
    h_flex()
        .id(id.into())
        .h(px(size::CONTROL))
        .gap(px(space::XXS))
        // The border is part of the gap.
        .px(px(gap - 1.))
        .items_center()
        .rounded(px(radii(cx).bar()))
        .bg(surface::glass(colors.elevated_surface_background, 0.92, cx))
        .border_1()
        .border_color(gpui::transparent_black())
        .shadow(elevation::custom(cx))
        .occlude()
}

/// The widest a [`tag`] grows before its label truncates.
pub const TAG_MAX_WIDTH: f32 = 160.;

/// A tag: a name in a quiet pill, as a table shows a row's neighbours. The UI font, unlike
/// `ui::Chip`, which is for code-like values such as enum members.
pub fn tag(label: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .flex_none()
        .max_w(px(TAG_MAX_WIDTH))
        .h(px(size::TAG))
        .px(px(space::SM))
        .flex()
        .items_center()
        .rounded(px(radii(cx).xs))
        .bg(surface::tag(cx))
        .child(crate::cell_text(label, cx).meta(cx).hug())
}

/// A code-like value in a quiet box, as a table shows an enum member: [`tag`] with the value
/// in the buffer font. Zed's `Chip` keeps its own corner and hairline.
pub fn chip(label: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .flex_none()
        .max_w(px(TAG_MAX_WIDTH))
        .h(px(size::TAG))
        .px(px(space::SM))
        .flex()
        .items_center()
        .rounded(px(radii(cx).xs))
        .bg(surface::tag(cx))
        // A cell text, not a truncating label: every enum cell draws one, and a truncating
        // label is shaped again on every pass of every frame.
        .child(crate::cell_text(label, cx).meta(cx).buffer_font(cx).hug())
}

/// A checkbox and its label: a box of the scale's extra-small corner, filled with the ink
/// and a check when on. Zed's `Checkbox` keeps a 2 px corner of its own. The caller handles
/// the click and keeps the state; a disabled one recedes and takes no pointer.
pub fn checkbox(
    id: impl Into<ElementId>,
    checked: bool,
    label: impl Into<SharedString>,
    disabled: bool,
    cx: &App,
) -> Stateful<Div> {
    let ink = cx.theme().colors().text;
    let mark = div()
        .size(px(size::CHECK))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(radii(cx).xs))
        .border_1();
    let mark = if checked {
        mark.bg(ink).border_color(ink).child(
            Icon::new(IconName::Check)
                .size(IconSize::XSmall)
                .color(Color::Custom(surface::document(cx))),
        )
    } else {
        mark.bg(surface::document(cx))
            .border_color(surface::control_border(cx))
    };
    h_flex()
        .id(id.into())
        .gap(px(space::SM))
        .items_center()
        .when(disabled, |row| row.opacity(opacity::DIMMED))
        .when(!disabled, |row| row.cursor_pointer())
        .child(mark)
        .child(text::body(label))
}

/// A severity's icon and its colour, shared by [`Banner`] and [`Callout`].
fn notice_icon(severity: ui::Severity) -> (IconName, Color) {
    match severity {
        ui::Severity::Info => (IconName::Info, Color::Muted),
        ui::Severity::Success => (IconName::Check, Color::Success),
        ui::Severity::Warning => (IconName::Warning, Color::Warning),
        ui::Severity::Error => (IconName::XCircle, Color::Error),
    }
}

/// A notice inside a document: its severity's icon, then what it says, and an action at its
/// end, in a box of the scale's small corner (Zed's `Banner`, on the design system's corners,
/// tones and spacing). Made by [`banner`].
#[derive(IntoElement)]
pub struct Banner {
    severity: ui::Severity,
    children: Vec<AnyElement>,
    action_slot: Option<AnyElement>,
    wrap_content: bool,
}

/// A [`Banner`] of `severity`.
pub fn banner(severity: ui::Severity) -> Banner {
    Banner {
        severity,
        children: Vec::new(),
        action_slot: None,
        wrap_content: false,
    }
}

impl Banner {
    pub fn action_slot(mut self, element: impl IntoElement) -> Self {
        self.action_slot = Some(element.into_any_element());
        self
    }

    /// Let the content and the action wrap onto lines of their own when narrow.
    pub fn wrap_content(mut self, wrap: bool) -> Self {
        self.wrap_content = wrap;
        self
    }
}

impl ParentElement for Banner {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements)
    }
}

impl RenderOnce for Banner {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let (icon_name, color) = notice_icon(self.severity);
        let (fill, border) = surface::notice(self.severity, cx);
        let action = self.action_slot.is_some();
        h_flex()
            .min_w_0()
            .py(px(space::XXS))
            .pl(px(space::MD))
            .pr(px(if action { space::XS } else { space::MD }))
            .gap(px(space::SM))
            .when(self.wrap_content, |banner| banner.flex_wrap())
            .justify_between()
            .rounded(px(radii(cx).sm))
            .border_1()
            .border_color(border)
            .bg(fill)
            .child(
                h_flex()
                    .items_start()
                    .min_w_0()
                    .flex_1()
                    .gap(px(space::SM))
                    .child(
                        h_flex()
                            .h(window.line_height())
                            .flex_none()
                            .child(icon::meta(icon_name).color(color)),
                    )
                    .child(div().min_w_0().flex_1().children(self.children)),
            )
            .children(self.action_slot)
    }
}

/// A notice across a panel or a thread, flush with its edges under a rule: its severity's
/// icon, a title with actions at its end, and a description that scrolls past
/// `size::NOTICE_MAX_HEIGHT` (Zed's `Callout`, on the design system's tones and spacing).
/// Made by [`callout`].
#[derive(IntoElement)]
pub struct Callout {
    severity: ui::Severity,
    title: SharedString,
    description: Option<AnyElement>,
    actions_slot: Option<AnyElement>,
}

/// A [`Callout`] of `severity` titled `title`.
pub fn callout(severity: ui::Severity, title: impl Into<SharedString>) -> Callout {
    Callout {
        severity,
        title: title.into(),
        description: None,
        actions_slot: None,
    }
}

impl Callout {
    /// A sentence under the title, muted.
    pub fn description(mut self, description: impl Into<SharedString>) -> Self {
        self.description = Some(text::muted(description).into_any_element());
        self
    }

    /// Any content under the title (a code block, a list of problems).
    pub fn description_slot(mut self, description: impl IntoElement) -> Self {
        self.description = Some(description.into_any_element());
        self
    }

    pub fn actions_slot(mut self, actions: impl IntoElement) -> Self {
        self.actions_slot = Some(actions.into_any_element());
        self
    }
}

impl RenderOnce for Callout {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let (icon_name, color) = notice_icon(self.severity);
        let (fill, _) = surface::notice(self.severity, cx);
        let line = window.line_height();
        // Keyed by its title: callouts side by side (settings.json's problems and
        // keymap.json's) scroll apart.
        let description_id = SharedString::from(format!("callout-{}", self.title));
        h_flex()
            .min_w_0()
            .w_full()
            .p(px(space::MD))
            .gap(px(space::MD))
            .items_start()
            .border_t_1()
            .border_color(surface::rule(cx))
            .bg(fill)
            .overflow_x_hidden()
            .child(
                h_flex()
                    .h(line)
                    .flex_none()
                    .child(icon::lead(icon_name).color(color)),
            )
            .child(
                v_flex()
                    .min_w_0()
                    .w_full()
                    .gap(px(space::XS))
                    .child(
                        h_flex()
                            .min_h(line)
                            .w_full()
                            .gap(px(space::XS))
                            .justify_between()
                            .flex_wrap()
                            .child(div().min_w_0().flex_1().child(text::body(self.title)))
                            .children(self.actions_slot),
                    )
                    .children(self.description.map(|description| {
                        div()
                            .id(description_id)
                            .w_full()
                            .max_h(px(size::NOTICE_MAX_HEIGHT))
                            .overflow_y_scroll()
                            .child(description)
                    })),
            )
    }
}

/// A choice of two to four, as Tower's tabs: no track, quiet labels, the selected one on the
/// pressed pill. Over content, put it in a [`float_bar`].
pub fn segmented(_cx: &App) -> Div {
    h_flex().flex_none().gap(px(space::XXS))
}

/// One option of a [`segmented`] control: a bar button's height, so a floating bar holds it.
pub fn segment(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    selected: bool,
    cx: &App,
) -> Stateful<Div> {
    let hover = surface::highlight(cx);
    h_flex()
        .id(id.into())
        .h(px(size::BAR_BUTTON))
        .px(px(space::MD + space::XXS))
        .items_center()
        .rounded(px(radii(cx).sm))
        .cursor_pointer()
        .when(selected, |segment| segment.bg(surface::pressed(cx)))
        .when(!selected, |segment| {
            segment.hover(move |style| style.bg(hover))
        })
        .child(
            Label::new(label.into())
                .size(LabelSize::Small)
                .weight(text::weight::BODY)
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
        .id(id)
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
                                row_icon_button("reset", IconName::Undo)
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
        .child(ui::KeyBinding::for_action_in(action, focus, cx).size(rems_from_px(text::KEY_HINT)))
        .child(text::meta(label))
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
        .rounded(px(radii(cx).sm))
        .border_1();
    let block = dotted(block, cx).bg(if is_glass(cx) {
        surface::well(cx)
    } else {
        surface::sunken(cx)
    });
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
        .min_h(px(size::FIELD_ROW))
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
/// interfaces.dev's `shadow-custom` on a surface: its ring outside the box and its two soft
/// drops ([`elevation::custom`]), over a clear 1 px border, so the layout keeps the hairline's
/// room.
pub fn lifted<E: Styled>(element: E, cx: &App) -> E {
    element
        .border_1()
        .border_color(gpui::transparent_black())
        .shadow(elevation::custom(cx))
}

pub fn card(cx: &App) -> Div {
    // interfaces.dev's card (`.preview-card`).
    lifted(
        v_flex()
            .w_full()
            .rounded(px(radii(cx).md))
            .bg(surface::document(cx))
            .overflow_hidden(),
        cx,
    )
}

pub fn card_header(cx: &App) -> Div {
    h_flex()
        .w_full()
        // Its fill meets the card's top corners.
        .rounded_t(px(radius::flush(radii(cx).md)))
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
        .border_color(surface::card_border(cx))
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
        .child(text::state(title))
}

/// What a view shows while its content is read, where the content will be: a muted line, as
/// Zed's panels say "Loading…". No icon: it passes, and asks nothing.
pub fn loading_state(title: impl Into<SharedString>) -> Div {
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .p(px(space::PAGE))
        .child(text::state(title))
}

/// A view whose content could not be read, laid out as an [`empty_state`]: an error icon, what
/// failed in a few words, then the reason as it came. Recovery actions follow as children.
pub fn error_state(title: impl Into<SharedString>, reason: impl Into<SharedString>) -> Div {
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .gap(px(space::LG))
        .p(px(space::PAGE))
        .child(
            Icon::new(IconName::XCircle)
                .size(IconSize::Medium)
                .color(Color::Error),
        )
        .child(
            v_flex()
                .items_center()
                .gap(px(space::XS))
                .child(Label::new(title.into()))
                .child(state_detail(reason)),
        )
}

/// One line under a state's title for what its words and actions cannot carry: a reason, or a
/// value such as a reference's address. Centered and wrapped at a reading width.
pub fn state_detail(text: impl Into<SharedString>) -> Div {
    div()
        .max_w(px(size::STATE_DETAIL_WIDTH))
        .text_center()
        .child(text::meta(text))
}

/// An action an empty or error state offers: an outlined button with its key, read in the
/// view's context. The caller says what a click does.
pub fn state_action(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    action: &dyn gpui::Action,
    focus: &gpui::FocusHandle,
    cx: &App,
) -> Button {
    button(id, label).style(ButtonStyle::Outlined).key_binding(
        ui::KeyBinding::for_action_in(action, focus, cx).size(rems_from_px(text::KEY_HINT)),
    )
}

/// A raised glass sheet: a translucent fill over the wash, a light outline, and layered soft
/// shadows instead of a border. GPUI clips its children to the sheet's rectangle, not its
/// rounded corners, so a child with a background flush in a corner takes the sheet's radius
/// there (`Radii::sheet`).
pub fn sheet(cx: &App) -> Div {
    div()
        .rounded(px(radii(cx).sheet()))
        .overflow_hidden()
        .bg(surface::glass(
            cx.theme().colors().panel_background,
            0.8,
            cx,
        ))
        .shadow(elevation::sheet(cx))
}

/// A rule between the rows or the sections of a document: the rule's hairline, dotted on
/// glass as Tower's feeds are. A region's edge (a toolbar's, a strip's) stays solid. Give the
/// element its border sides; this sets their colour and style.
pub fn dotted<E: Styled>(element: E, cx: &App) -> E {
    let element = element.border_color(surface::rule(cx));
    if is_glass(cx) {
        element.border_dashed()
    } else {
        element
    }
}

/// Text sizes for a container whose text a view does not set itself (a markdown reply, an
/// editor, a run of text children), as [`text`] sets a label's.
pub trait Prose: Styled + Sized {
    /// Reading text: an agent's message, the composer: the default size, Tower's `text-sm`.
    fn prose(self) -> Self {
        self.text_sm()
    }

    /// Quiet reading text (an agent's thoughts): the small size, muted.
    fn quiet_prose(self, cx: &App) -> Self {
        self.text_xs().text_color(Color::Muted.color(cx))
    }
}

impl<E: Styled> Prose for E {}

/// [`dotted`] as a method, for a chain: `.border_b_1().dotted(cx)`.
pub trait Dotted: Styled + Sized {
    fn dotted(self, cx: &App) -> Self {
        dotted(self, cx)
    }
}

impl<E: Styled> Dotted for E {}

/// The rule under a list row: [`dotted`] below it.
pub fn row_rule(row: Div, cx: &App) -> Div {
    dotted(row.border_b_1(), cx)
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
            .children(self.place_icon.map(icon::meta))
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

/// A rounded-full control with a hairline: a picker in a composer ("Claude ▾").
pub fn pill(id: impl Into<ElementId>, cx: &App) -> Stateful<Div> {
    let hover = surface::highlight(cx);
    h_flex()
        .id(id.into())
        .h(px(size::PILL))
        .px(px(space::MD))
        .gap(px(space::XS))
        .items_center()
        .rounded_full()
        .border_1()
        .border_color(surface::control_border(cx))
        .cursor_pointer()
        .hover(move |style| style.bg(hover))
}

/// A composer's icon action (send, stop): a control-sized square at the scale's corner, solid
/// ink when it is the action to take.
pub fn composer_button(
    id: impl Into<ElementId>,
    icon: IconName,
    filled: bool,
    cx: &App,
) -> Stateful<Div> {
    let colors = cx.theme().colors();
    let hover = surface::highlight(cx);
    div()
        .id(id.into())
        .size(px(size::CONTROL))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(radii(cx).sm))
        .cursor_pointer()
        .map(|button| {
            if filled {
                button.bg(colors.text).child(
                    Icon::new(icon)
                        .size(IconSize::Small)
                        .color(Color::Custom(surface::document(cx))),
                )
            } else {
                button
                    .border_1()
                    .border_color(surface::control_border(cx))
                    .hover(move |style| style.bg(hover))
                    .child(icon::lead(icon))
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A glass sheet or toast keeps the light style's warm white in a light theme, and takes the
    /// dark theme's own surface in a dark one: near-white there would put light text on light.
    /// What glass lays over a sheet (a rule, a well, a selected or hovered row, an inactive tab)
    /// shows on both: black ink on the white, white ink on the dark surface, where black moved
    /// Studio Dark's sheet by less than a level.
    #[test]
    fn glass_takes_the_dark_themes_surface_and_its_states_show_on_it() {
        let dark_panel = gpui::hsla(0.6, 0.1, 0.16, 1.);
        let light = surface::glass_fill(true, dark_panel, 0.8);
        assert!(light.l > 0.95, "the light style's white");
        assert!((light.a - 0.8).abs() < 1e-6);
        let dark = surface::glass_fill(false, dark_panel, 0.8);
        assert_eq!(dark.l, dark_panel.l, "the theme's surface");
        assert!((dark.a - 0.8).abs() < 1e-6);

        // Studio Dark's panel, under its sheet.
        let studio_dark: Hsla = gpui::rgb(0x101010).into();
        let states = [
            ("a rule", surface::RULE_INK),
            ("a well", surface::WELL_INK),
            ("a selected row", surface::SELECTED_INK),
            ("a hovered row", surface::HOVER_INK),
            ("an inactive tab", surface::TAB_INK),
        ];
        for light in [true, false] {
            let sheet = surface::glass_fill(light, studio_dark, 1.);
            for (state, alpha) in states {
                let levels = (sheet.blend(surface::ink(light, alpha)).l - sheet.l).abs() * 255.;
                assert!(
                    levels >= 3.,
                    "{state} moves a {} sheet {levels:.1} levels",
                    if light { "light" } else { "dark" }
                );
            }
        }
        let black = studio_dark.blend(gpui::black().opacity(surface::SELECTED_INK));
        assert!(
            (black.l - studio_dark.l).abs() * 255. < 1.,
            "black ink is lost on a dark sheet"
        );
    }

    /// The nested pairs the design system draws stay concentric (`radius::around`/`within`),
    /// at every base the corner radius setting allows.
    #[test]
    fn nested_radii_are_concentric() {
        // The default base draws the radii Studio was designed at.
        let default = Radii::new(radius::DEFAULT_BASE);
        assert_eq!(
            (
                default.xs,
                default.sm,
                default.md,
                default.lg,
                default.modal
            ),
            (4., 6., 10., 14., 16.)
        );
        assert_eq!(default.menu_item(), 5.);
        assert_eq!((default.palette_list(), default.palette_row()), (11., 6.));
        assert_eq!(default.row_list(), 9.);
        for base in 1..=20 {
            let r = Radii::new(base as f32);
            // A card `space::XS` inside a glass sheet; a chip `space::SM` inside a card; a
            // card inside a modal's `space::SM`.
            assert_eq!(radius::within(r.lg, space::XS), r.md, "base {base}");
            assert_eq!(radius::within(r.md, space::SM), r.xs, "base {base}");
            assert_eq!(radius::within(r.modal, space::SM), r.md, "base {base}");
            // A menu's rows, the palette's results and their rows: each inside a hairline
            // and `space::XS` of padding; a row list around its rows.
            let inset = space::XS + size::HAIRLINE;
            assert_eq!(r.menu_item(), radius::within(r.menu(), inset));
            assert_eq!(r.palette_list(), radius::within(r.modal, inset));
            assert_eq!(r.palette_row(), radius::within(r.palette_list(), inset));
            assert_eq!(r.row_list(), radius::around(r.xs, inset));
        }
        // 0 squares everything.
        let square = Radii::new(0.);
        assert_eq!(
            (square.xs, square.sm, square.md, square.lg, square.modal),
            (0., 0., 0., 0., 0.)
        );
        // A fill flush with a bordered edge: a card's header, a modal's footer.
        assert_eq!(radius::flush(default.md), 9.);
        assert_eq!(radius::flush(default.modal), 15.);
        // A pill button's half height is the pill's less the gap: round in its round end.
        let button = size::PILL - 2. * PILL_BUTTON_GAP;
        assert_eq!(
            radius::within(size::PILL / 2., PILL_BUTTON_GAP),
            button / 2.
        );
        // A gap wider than the radius leaves the child square.
        assert_eq!(radius::within(default.xs, space::XL), 0.);
    }
}
