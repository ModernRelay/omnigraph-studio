//! One line of text, cut with "…" when it does not fit: what a grid cell, a tag or a count
//! beside them shows. A text element measures its text for the layout, and a truncated one
//! measures it on every pass of every frame (GPUI's `TextLayout::layout` keeps no size for
//! it), taking a line wrapper from the shared pool each time. This element is never measured:
//! it fills the room its parent gives it (a cell's box, fixed by its column), or asks for the
//! width of its line and gives way when there is less ([`CellText::hug`]). It shapes its line
//! once a frame (GPUI keeps shaped lines from the frame before) and cuts it only when it is
//! wider than its room.
//!
//! It looks as a small `Label` does: the UI font at the small size and weight, in the colour
//! given, or the buffer font.

use gpui::{
    App, Bounds, DefiniteLength, Element, ElementId, FontWeight, GlobalElementId, Hsla,
    InspectorElementId, IntoElement, LayoutId, Length, Pixels, ShapedLine, SharedString, Style,
    TextAlign, TextStyleRefinement, TruncateFrom, Window, px,
};

/// See the module documentation.
pub struct CellText {
    text: SharedString,
    align: TextAlign,
    hug: bool,
    style: TextStyleRefinement,
}

/// `text` on one line, from the start of its room, as a small label draws it. A control
/// character (a line break, a tab) shows as its picture, as a single-line label has it.
pub fn cell_text(text: impl Into<SharedString>, cx: &App) -> CellText {
    CellText {
        text: one_line(text.into()),
        align: TextAlign::Left,
        hug: false,
        style: TextStyleRefinement {
            color: Some(ui::Color::Default.color(cx)),
            font_size: Some(crate::ds::text::body_size(cx).into()),
            font_weight: Some(theme::theme_settings(cx).ui_font(cx).weight),
            ..TextStyleRefinement::default()
        },
    }
}

impl CellText {
    pub fn color(mut self, color: Hsla) -> Self {
        self.style.color = Some(color);
        self
    }

    /// At the metadata size, as a `meta` label beside it.
    pub fn meta(mut self, cx: &App) -> Self {
        self.style.font_size = Some(crate::ds::text::meta_size(cx).into());
        self
    }

    /// The buffer font, at its weight, as `Label::buffer_font` sets it.
    pub fn buffer_font(mut self, cx: &App) -> Self {
        let font = theme::theme_settings(cx).buffer_font(cx).clone();
        self.style.font_family = Some(font.family);
        self.style.font_features = Some(font.features);
        self.style.font_fallbacks = font.fallbacks;
        self.style.font_weight = Some(font.weight);
        self.style.font_style = Some(font.style);
        self
    }

    pub fn weight(mut self, weight: FontWeight) -> Self {
        self.style.font_weight = Some(weight);
        self
    }

    /// Set against the end of its room, as numbers are.
    pub fn align_end(mut self) -> Self {
        self.align = TextAlign::Right;
        self
    }

    /// As wide as its line, narrower (and cut) when its parent has less room, as a tag is.
    pub fn hug(mut self) -> Self {
        self.hug = true;
        self
    }
}

impl IntoElement for CellText {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

/// The line as it fits, and the height it is drawn in.
pub struct Fitted {
    line: ShapedLine,
    line_height: Pixels,
}

impl Element for CellText {
    /// A hugging line, shaped to ask for its width.
    type RequestLayoutState = Option<ShapedLine>;
    type PrepaintState = Option<Fitted>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    /// One line high; the rest of the parent's row, or the line's width, and no narrower than
    /// nothing. Never measured.
    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Option<ShapedLine>) {
        let (line_height, whole) = window.with_text_style(Some(self.style.clone()), |window| {
            let whole = self.hug.then(|| self.shape(window));
            (line_height(window), whole)
        });
        let none = Length::Definite(DefiniteLength::Absolute(px(0.).into()));
        let mut style = match &whole {
            Some(whole) => {
                let mut style = Style {
                    flex_shrink: 1.,
                    ..Style::default()
                };
                style.size.width = Length::Definite(DefiniteLength::Absolute(whole.width.into()));
                style
            }
            None => Style {
                flex_grow: 1.,
                flex_shrink: 1.,
                flex_basis: none,
                ..Style::default()
            },
        };
        style.min_size.width = none;
        style.size.height = Length::Definite(DefiniteLength::Absolute(line_height.into()));
        (window.request_layout(style, [], cx), whole)
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        whole: &mut Option<ShapedLine>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Fitted> {
        if self.text.is_empty() {
            return None;
        }
        window.with_text_style(Some(self.style.clone()), |window| {
            let whole = whole.take().unwrap_or_else(|| self.shape(window));
            self.fit(whole, bounds.size.width, window, cx)
        })
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _whole: &mut Option<ShapedLine>,
        fitted: &mut Option<Fitted>,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let Some(fitted) = fitted {
            fitted
                .line
                .paint(
                    bounds.origin,
                    fitted.line_height,
                    self.align,
                    Some(bounds.size.width),
                    window,
                    cx,
                )
                .ok();
        }
    }
}

impl CellText {
    /// The whole line, shaped in the text style in effect.
    fn shape(&self, window: &mut Window) -> ShapedLine {
        let text_style = window.text_style();
        let font_size = text_style.font_size.to_pixels(window.rem_size());
        window.text_system().shape_line(
            self.text.clone(),
            font_size,
            &[text_style.to_run(self.text.len())],
            None,
        )
    }

    /// `whole`, or cut to `width` when it is wider.
    fn fit(
        &self,
        whole: ShapedLine,
        width: Pixels,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Fitted> {
        let line = if whole.width <= width {
            whole
        } else {
            // Wider than its room, as few are: cut at the end, "…" taking the place of what
            // does not show.
            let text_style = window.text_style();
            let font_size = text_style.font_size.to_pixels(window.rem_size());
            let runs = [text_style.to_run(self.text.len())];
            let (text, runs) = cx
                .text_system()
                .line_wrapper(text_style.font(), font_size)
                .truncate_line(self.text.clone(), width, "…", &runs, TruncateFrom::End);
            window
                .text_system()
                .shape_line(text, font_size, &runs, None)
        };
        Some(Fitted {
            line,
            line_height: line_height(window),
        })
    }
}

/// `text` with each control character as its picture: GPUI shapes one line only.
fn one_line(text: SharedString) -> SharedString {
    match ui::utils::replace_control_characters(&text) {
        std::borrow::Cow::Owned(replaced) => replaced.into(),
        std::borrow::Cow::Borrowed(_) => text,
    }
}

/// The height of a line of the text style in effect, on whole pixels as GPUI's text is.
fn line_height(window: &Window) -> Pixels {
    let text_style = window.text_style();
    let font_size = text_style.font_size.to_pixels(window.rem_size());
    window.pixel_snap(
        text_style
            .line_height
            .to_pixels(font_size.into(), window.rem_size()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_with_line_breaks_is_drawn_on_one_line() {
        assert_eq!(one_line("plain".into()), SharedString::from("plain"));
        let shown = one_line("first\nsecond\tthird".into());
        assert!(!shown.contains('\n') && !shown.contains('\t'), "{shown}");
        assert!(
            shown.starts_with("first") && shown.ends_with("third"),
            "{shown}"
        );
    }
}
