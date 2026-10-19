//! Modals (spec 002, "Modals"), after Tower's dialogs: a floating card over a dim, a header
//! with the title, what it is about and a round close button, a body, and a footer strip
//! holding the actions on the right — Cancel as an outline pill, the action solid. Every
//! modal is built from these, so they share their width, radius, insets and type.
//!
//! The workspace draws the dim and places the card a quarter of the way down the free space
//! (`studio_workspace`'s modal layer); a modal draws only its card.

use gpui::{AnyElement, App, ClickEvent, Div, ElementId, SharedString, Window};
use ui::Tooltip;
use ui::prelude::*;

use crate::button::{Button, Variant};
use crate::ds::{elevation, radii, radius, size, space, surface};

/// A modal's card: the floating fill, its hairline, the floating shadow and the modal radius,
/// [`size::MODAL_WIDTH`] wide. Children are a [`ModalHeader`], [`modal_body`] and
/// [`modal_footer`]; the card clips them to its corners.
pub fn modal(cx: &App) -> Div {
    v_flex()
        .w(px(size::MODAL_WIDTH))
        .max_w_full()
        .rounded(px(radii(cx).modal))
        .bg(surface::floating(cx))
        .border_1()
        .border_color(surface::floating_border(cx))
        .shadow(elevation::floating(cx))
        .overflow_hidden()
}

type CloseHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

/// A modal's header: the title, what the modal is about, an element beside the title (the
/// branch it writes to), and the close button.
#[derive(IntoElement)]
pub struct ModalHeader {
    title: SharedString,
    description: Option<SharedString>,
    meta: Option<AnyElement>,
    on_close: Option<CloseHandler>,
    close_disabled: bool,
}

impl ModalHeader {
    pub fn new(title: impl Into<SharedString>) -> Self {
        Self {
            title: title.into(),
            description: None,
            meta: None,
            on_close: None,
            close_disabled: false,
        }
    }

    /// One or two sentences under the title, muted.
    pub fn description(mut self, description: impl Into<Option<SharedString>>) -> Self {
        self.description = description.into();
        self
    }

    /// Beside the title, before the close button: what the modal writes to.
    pub fn meta(mut self, meta: impl IntoElement) -> Self {
        self.meta = Some(meta.into_any_element());
        self
    }

    /// The round close button, top right.
    pub fn on_close(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_close = Some(Box::new(handler));
        self
    }

    /// Fades the close button while the modal may not close (a write is running).
    pub fn close_disabled(mut self, disabled: bool) -> Self {
        self.close_disabled = disabled;
        self
    }
}

impl RenderOnce for ModalHeader {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        v_flex()
            .px(px(space::MODAL))
            .pt(px(space::MODAL - space::XS))
            .pb(px(space::MODAL_STACK))
            .gap(px(space::SM))
            .child(
                h_flex()
                    .gap(px(space::MD))
                    .items_center()
                    .child(
                        div().min_w_0().flex_1().child(
                            Label::new(self.title)
                                .size(LabelSize::Large)
                                .weight(crate::ds::text::weight::HEADING)
                                .single_line()
                                .truncate(),
                        ),
                    )
                    .children(self.meta)
                    .children(self.on_close.map(|on_close| {
                        crate::ds::sheet_close_button("modal-close")
                            .tooltip(Tooltip::text("Close"))
                            .disabled(self.close_disabled)
                            .on_click(on_close)
                    })),
            )
            .children(self.description.map(crate::ds::text::muted))
    }
}

/// A modal's body: the form or the list, inset as the header is.
pub fn modal_body() -> Div {
    v_flex()
        .px(px(space::MODAL))
        .pb(px(space::MODAL_STACK + space::XS))
        .gap(px(space::MODAL_STACK))
}

/// A modal's footer: a strip under a hairline, its actions on the right; a note or a toggle
/// may lead it on the left.
pub fn modal_footer(cx: &App) -> Div {
    h_flex()
        .px(px(space::MODAL))
        .py(px(space::LG))
        .gap(px(space::MD))
        .items_center()
        .justify_end()
        .border_t_1()
        .border_color(surface::floating_border(cx))
        .bg(cx.theme().colors().text.opacity(0.025))
        // Its fill meets the card's bottom corners.
        .rounded_b(px(radius::flush(radii(cx).modal)))
}

/// A modal's Cancel: an outline pill at a control's height.
pub fn cancel_button(id: impl Into<ElementId>) -> Button {
    Button::new(id, "Cancel").variant(Variant::Outline)
}

/// A modal's action: solid ink; [`Variant::Destructive`] for one that deletes.
pub fn action_button(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Button {
    Button::new(id, label).variant(Variant::Primary)
}

/// The branch a modal writes to, beside its title: a quiet pill.
pub fn write_target(branch: impl Into<SharedString>, cx: &App) -> Div {
    let branch: SharedString = branch.into();
    h_flex()
        .flex_none()
        .h(px(size::TAG))
        .px(px(space::MD))
        .gap(px(space::XS))
        .items_center()
        .rounded(px(radii(cx).xs))
        .bg(surface::pressed(cx))
        .child(crate::ds::icon::meta(IconName::GitBranch))
        .child(crate::ds::text::meta(branch).single_line())
}
