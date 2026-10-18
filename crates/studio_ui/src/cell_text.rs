//! One line of text that fills the room its parent gives it, cut with "…" when it does not
//! fit: what a grid cell shows. A text element measures its text for the layout, and a
//! truncated one measures it on every pass of every frame (GPUI's `TextLayout::layout` keeps no
//! size for it), taking a line wrapper from the shared pool each time. A cell's box is fixed by
//! its column, so this element asks for no measurement: it takes the room, shapes its line
//! once a frame (GPUI keeps shaped lines from the frame before), and cuts it only when it is
//! wider than that room.
//!
//! It looks as a small `Label` does: the UI font at the small size and weight, in the colour
//! given, or the buffer font.

use gpui::{
    App, Bounds, DefiniteLength, Element, ElementId, FontWeight, GlobalElementId, Hsla,
    InspectorElementId, IntoElement, LayoutId, Length, Pixels, ShapedLine, SharedString, Style,
    TextAlign, TextStyleRefinement, TruncateFrom, Window, px,
};
use ui::TextSize;

/// See the module documentation.
pub struct CellText {
    text: SharedString,
    align: TextAlign,
    style: TextStyleRefinement,
}

/// `text` on one line, from the start of its room, as a small label draws it.
pub fn cell_text(text: impl Into<SharedString>, cx: &App) -> CellText {
    CellText {
        text: text.into(),
        align: TextAlign::Left,
        style: TextStyleRefinement {
            font_size: Some(TextSize::Small.rems(cx).into()),
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
    type RequestLayoutState = ();
    type PrepaintState = Option<Fitted>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    /// The rest of the parent's row, one line high, and no narrower than nothing: never
    /// measured.
    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let line_height =
            window.with_text_style(Some(self.style.clone()), |window| line_height(window));
        let none = Length::Definite(DefiniteLength::Absolute(px(0.).into()));
        let mut style = Style {
            flex_grow: 1.,
            flex_shrink: 1.,
            flex_basis: none,
            ..Style::default()
        };
        style.min_size.width = none;
        style.size.height = Length::Definite(DefiniteLength::Absolute(line_height.into()));
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Fitted> {
        if self.text.is_empty() {
            return None;
        }
        window.with_text_style(Some(self.style.clone()), |window| {
            self.fit(bounds.size.width, window, cx)
        })
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut (),
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
    /// The line shaped in the text style in effect, cut to `width` when it is wider.
    fn fit(&self, width: Pixels, window: &mut Window, cx: &mut App) -> Option<Fitted> {
        let text_style = window.text_style();
        let font_size = text_style.font_size.to_pixels(window.rem_size());
        let runs = [text_style.to_run(self.text.len())];
        let whole = window
            .text_system()
            .shape_line(self.text.clone(), font_size, &runs, None);
        let line = if whole.width <= width {
            whole
        } else {
            // Wider than its room, as few cells are: cut at the end, "…" taking the place of
            // what does not show.
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
