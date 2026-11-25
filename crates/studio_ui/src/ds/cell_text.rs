//! One line of text, cut with "…" when it does not fit: what a grid cell, a tag or a count
//! beside them shows. A text element measures its text for the layout, and a truncated one
//! measures it on every pass of every frame (GPUI's `TextLayout::layout` keeps no size for
//! it), taking a line wrapper from the shared pool each time. This element is never measured:
//! it fills the room its parent gives it (a cell's box, fixed by its column), or asks for the
//! width of its line and gives way when there is less ([`CellText::hug`]). It shapes its line
//! once a frame (GPUI keeps shaped lines from the frame before) and cuts it only when it is
//! wider than its room. It works out its text style once a frame: GPUI makes a style from
//! every refinement on its stack each time it is asked (`Window::text_style`), and allocates as
//! it does, so the line is shaped as it is laid out and kept for the steps after.
//!
//! It looks as a small `Label` does: the UI font at the small size and weight, in the colour
//! given, or the buffer font.
//!
//! Wrapped ([`CellText::wrap`], spec 049), it runs onto as many lines as the width it is given
//! needs, at most [`crate::WRAP_LINES`], the last cut with "…", and asks for their height. The
//! width is the column's, known before layout, so it is still never measured.

use gpui::{
    App, Bounds, DefiniteLength, Element, ElementId, FontWeight, GlobalElementId, Hsla,
    InspectorElementId, IntoElement, LayoutId, Length, Pixels, ShapedLine, SharedString, Style,
    TextAlign, TextStyle, TextStyleRefinement, TruncateFrom, Window, WrappedLine, px,
};

/// See the module documentation.
pub struct CellText {
    text: SharedString,
    align: TextAlign,
    hug: bool,
    /// The width its lines wrap at, when it wraps ([`CellText::wrap`]).
    wrap: Option<Pixels>,
    pub(crate) style: TextStyleRefinement,
}

/// The most of a text one line shows: a column is at most `COLUMN_MAX_WIDTH` wide, so the rest
/// of a long value (a paragraph in a Text cell) is never copied, scanned or shaped.
pub(crate) const SHOWN_CHARS: usize = 512;

/// `text` on one line, from the start of its room, as a small label draws it. A control
/// character (a line break, a tab) shows as its picture, as a single-line label has it.
pub fn cell_text(text: impl Into<SharedString>, cx: &App) -> CellText {
    CellText {
        text: one_line(shown_part(text.into())),
        align: TextAlign::Left,
        hug: false,
        wrap: None,
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

    /// Struck through, as a value that is gone is.
    pub fn struck(mut self) -> Self {
        self.style.strikethrough = Some(gpui::StrikethroughStyle {
            thickness: px(crate::ds::size::HAIRLINE),
            color: None,
        });
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

    /// Wrapped at `width` (its room's, which a grid knows from the column) onto at most
    /// [`crate::WRAP_LINES`] lines, the last cut with "…", as tall as the lines it takes.
    pub fn wrap(mut self, width: Pixels) -> Self {
        self.wrap = Some(width.max(px(1.)));
        self
    }

    /// How many lines it takes wrapped at `width` ([`CellText::wrap`]), at most
    /// [`crate::WRAP_LINES`]: what a grid sizes a wrapped row by when its cells cannot stretch
    /// to it. Shaped as the cell shapes it, so the cell drawn after takes it from GPUI's cache.
    pub fn wrapped_lines(&self, width: Pixels, window: &Window, cx: &App) -> usize {
        use gpui::Refineable as _;

        if self.text.is_empty() {
            return 1;
        }
        let mut text_style = window.text_style();
        text_style.refine(&self.style);
        let font_size = text_style.font_size.to_pixels(window.rem_size());
        self.shape_wrapped(&text_style, font_size, width.max(px(1.)), window, cx)
            .map_or(1, |wrapped| wrapped_lines(&wrapped))
    }

    /// How wide its line is when it has the room: what fitting a column to its values
    /// measures (spec 049). Shaped as the cell shapes it, so a frame that draws the cell after
    /// takes the line from GPUI's cache.
    pub fn line_width(&self, window: &Window) -> Pixels {
        use gpui::Refineable as _;

        let mut text_style = window.text_style();
        text_style.refine(&self.style);
        let font_size = text_style.font_size.to_pixels(window.rem_size());
        self.shape(&text_style, font_size, window).width
    }
}

impl IntoElement for CellText {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

/// The line as it fits, and the height it is drawn in. A shaped line is about 3 KB (its runs
/// are held inline), and GPUI moves an element's states from step to step of every frame: boxed,
/// a cell moves a pointer, not the line, which in a table was a tenth of a scroll frame
/// (`docs/drawing.md`).
pub struct Fitted {
    line: Box<ShapedLine>,
    line_height: Pixels,
}

/// What laying the text out found, kept for its prepaint: the whole line (for a hugging one,
/// also its width), and the height it is drawn in. Shaped as it is laid out, so a frame works
/// out the text style once, not again to shape it and again for its height.
pub struct Shaped {
    whole: Option<Box<Shape>>,
    line_height: Pixels,
}

/// The text as laid out: one line, cut later if it does not fit, or wrapped, already within
/// its lines. Always boxed ([`Shaped`]), so the line's size is the box's to carry.
#[expect(clippy::large_enum_variant)]
enum Shape {
    Line(ShapedLine),
    Wrapped(WrappedLine),
}

impl Element for CellText {
    type RequestLayoutState = Shaped;
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
    ) -> (LayoutId, Shaped) {
        let shaped = window.with_text_style(Some(self.style.clone()), |window| {
            let text_style = window.text_style();
            let font_size = text_style.font_size.to_pixels(window.rem_size());
            // A hugging line asks for its width, even an empty one's; an empty line in a cell
            // draws nothing.
            let whole = match self.wrap {
                Some(width) if !self.text.is_empty() => self
                    .shape_wrapped(&text_style, font_size, width, window, cx)
                    .map(|wrapped| Box::new(Shape::Wrapped(wrapped))),
                _ => (self.hug || !self.text.is_empty())
                    .then(|| Box::new(Shape::Line(self.shape(&text_style, font_size, window)))),
            };
            Shaped {
                whole,
                line_height: line_height(&text_style, font_size, window),
            }
        });
        let lines = match shaped.whole.as_deref() {
            Some(Shape::Wrapped(wrapped)) => wrapped_lines(wrapped),
            _ => 1,
        };
        let none = Length::Definite(DefiniteLength::Absolute(px(0.).into()));
        let hugged = match shaped.whole.as_deref() {
            Some(Shape::Line(whole)) if self.hug => Some(whole),
            _ => None,
        };
        let mut style = match hugged {
            Some(whole) => {
                let mut style = Style {
                    flex_shrink: 1.,
                    ..Style::default()
                };
                // Up to a whole device pixel: a size set is rounded to the nearest, which can
                // leave the line a fraction wider than its room, and cut it at every frame.
                let scale = window.scale_factor();
                let width = px((f32::from(whole.width) * scale).ceil() / scale);
                style.size.width = Length::Definite(DefiniteLength::Absolute(width.into()));
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
        style.size.height = Length::Definite(DefiniteLength::Absolute(
            (shaped.line_height * lines as f32).into(),
        ));
        (window.request_layout(style, [], cx), shaped)
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        shaped: &mut Shaped,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Fitted> {
        if self.text.is_empty() {
            return None;
        }
        // Wrapped lines are within their width already, and painted from the layout's state.
        if matches!(shaped.whole.as_deref(), Some(Shape::Wrapped(_))) {
            return None;
        }
        let Shape::Line(whole) = *shaped.whole.take()? else {
            return None;
        };
        let whole = Box::new(whole);
        let line = if whole.width <= bounds.size.width {
            whole
        } else {
            // Wider than its room, as few are: cut at the end, "…" taking the place of what
            // does not show.
            window.with_text_style(Some(self.style.clone()), |window| {
                self.cut(bounds.size.width, window, cx)
            })
        };
        Some(Fitted {
            line,
            line_height: shaped.line_height,
        })
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        shaped: &mut Shaped,
        fitted: &mut Option<Fitted>,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let Some(Shape::Wrapped(wrapped)) = shaped.whole.as_deref() {
            wrapped
                .paint(
                    bounds.origin,
                    shaped.line_height,
                    self.align,
                    Some(bounds),
                    window,
                    cx,
                )
                .ok();
            return;
        }
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
    /// The whole line, shaped in `text_style`.
    fn shape(&self, text_style: &TextStyle, font_size: Pixels, window: &Window) -> ShapedLine {
        shape_line(&self.text, text_style, font_size, window)
    }

    /// The text wrapped at `width` onto at most [`crate::WRAP_LINES`] lines, the last cut with
    /// "…" when there is more. The text is one line ([`one_line`]), so it shapes as one
    /// wrapped line.
    fn shape_wrapped(
        &self,
        text_style: &TextStyle,
        font_size: Pixels,
        width: Pixels,
        window: &Window,
        cx: &App,
    ) -> Option<WrappedLine> {
        let most = crate::WRAP_LINES;
        let runs = [text_style.to_run(self.text.len())];
        let shape = |text: SharedString, runs: &[gpui::TextRun], clamp: Option<usize>| {
            window
                .text_system()
                .shape_text(text, font_size, runs, Some(width), clamp)
                .ok()?
                .into_iter()
                .next()
        };
        let wrapped = shape(self.text.clone(), &runs, None)?;
        if wrapped.wrap_boundaries().len() < most {
            return Some(wrapped);
        }
        let (text, runs) = cx
            .text_system()
            .line_wrapper(text_style.font(), font_size)
            .truncate_wrapped_line(
                self.text.clone(),
                width,
                most,
                "…",
                &runs,
                TruncateFrom::End,
            );
        // The cut is measured by advances, which may still leave a line over: never more
        // lines than the most.
        shape(text, &runs, Some(most))
    }

    /// The line cut to `width` at its end, in the text style in effect.
    fn cut(&self, width: Pixels, window: &mut Window, cx: &mut App) -> Box<ShapedLine> {
        Box::new(cut_line(&self.text, width, window, cx))
    }
}

/// `text`, shaped whole in `text_style`.
pub(crate) fn shape_line(
    text: &SharedString,
    text_style: &TextStyle,
    font_size: Pixels,
    window: &Window,
) -> ShapedLine {
    window.text_system().shape_line(
        text.clone(),
        font_size,
        &[text_style.to_run(text.len())],
        None,
    )
}

/// `text` cut to `width` at its end, "…" taking the place of what does not show, in the text
/// style in effect.
pub(crate) fn cut_line(
    text: &SharedString,
    width: Pixels,
    window: &mut Window,
    cx: &mut App,
) -> ShapedLine {
    let text_style = window.text_style();
    let font_size = text_style.font_size.to_pixels(window.rem_size());
    let runs = [text_style.to_run(text.len())];
    let (text, runs) = cx
        .text_system()
        .line_wrapper(text_style.font(), font_size)
        .truncate_line(text.clone(), width, "…", &runs, TruncateFrom::End);
    window
        .text_system()
        .shape_line(text, font_size, &runs, None)
}

/// The start of `text` one line can show ([`SHOWN_CHARS`]), found without reading the rest: what
/// a view that keeps a cell's text keeps of it. A text cut here ends in "…", so a wrapped cell
/// with room for all of it still says there is more (spec 049).
pub fn shown_part(text: SharedString) -> SharedString {
    match text.char_indices().nth(SHOWN_CHARS) {
        Some((end, _)) => format!("{}…", &text[..end]).into(),
        None => text,
    }
}

/// `text` with each control character as its picture: GPUI shapes one line only.
pub(crate) fn one_line(text: SharedString) -> SharedString {
    match ui::utils::replace_control_characters(&text) {
        std::borrow::Cow::Owned(replaced) => replaced.into(),
        std::borrow::Cow::Borrowed(_) => text,
    }
}

/// How many lines a wrapped text is drawn on: never more than [`crate::WRAP_LINES`].
fn wrapped_lines(wrapped: &WrappedLine) -> usize {
    (wrapped.wrap_boundaries().len() + 1).min(crate::WRAP_LINES)
}

/// The height of a line of `text_style`, on whole pixels as GPUI's text is.
pub(crate) fn line_height(text_style: &TextStyle, font_size: Pixels, window: &Window) -> Pixels {
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
    fn a_long_value_is_kept_to_what_a_line_shows() {
        let long: String = "é".repeat(SHOWN_CHARS * 4);
        let shown = shown_part(long.into());
        assert_eq!(shown.chars().count(), SHOWN_CHARS + 1);
        assert!(shown.ends_with('…'), "a cut text says so");
        let short = SharedString::from("Ada");
        assert_eq!(shown_part(short.clone()), short);
    }

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

    /// GPUI moves an element and its states from step to step of every frame, for each of a
    /// table's hundreds of cells: they stay small, the shaped line behind a pointer.
    #[test]
    fn a_cell_moves_a_pointer_not_its_line() {
        assert!(
            std::mem::size_of::<gpui::ShapedLine>() > 1024,
            "a line is large"
        );
        assert!(std::mem::size_of::<Shaped>() <= 16);
        assert!(std::mem::size_of::<Option<Fitted>>() <= 16);
        assert!(std::mem::size_of::<CellText>() <= 256);
    }
}
