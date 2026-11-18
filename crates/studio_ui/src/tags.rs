//! A row of tags and a count after them, as one element: what a table's relationship cell
//! shows ("Ada  Grace  +3"). Drawn as boxes, each tag was a box and a text, inside a row that
//! clips them, inside a row with the count: two layout nodes a tag and three more a cell, in
//! every relationship cell of every frame. This element is one layout node. It shapes its lines
//! as it is laid out, places the tags and the count as those boxes would, on the same device
//! pixels, and paints each tag's quad and line itself.
//!
//! It looks as [`crate::ds::tag`]s in a row clipped at its end, then the count, does.

use gpui::{
    App, Bounds, DefiniteLength, Element, ElementId, GlobalElementId, Hsla, InspectorElementId,
    IntoElement, LayoutId, Length, Pixels, Point, ShapedLine, SharedString, Size, Style, TextAlign,
    TextStyleRefinement, Window, px,
};
use smallvec::SmallVec;

use crate::cell_text::{cut_line, line_height, shape_line};
use crate::ds::{TAG_MAX_WIDTH, size, space};

/// See the module documentation.
pub struct Tags {
    tags: SmallVec<[SharedString; 2]>,
    count: Option<SharedString>,
    tag_style: TextStyleRefinement,
    count_style: TextStyleRefinement,
    background: Hsla,
    radius: Pixels,
}

/// `tags` in quiet pills, then `count` in the muted body text when there is one. The tags give
/// way at the end of the room, the count does not.
pub fn tags(
    tags: impl IntoIterator<Item = SharedString>,
    count: Option<SharedString>,
    cx: &App,
) -> Tags {
    let body = crate::cell_text("", cx);
    Tags {
        tags: tags.into_iter().map(crate::cell_text::one_line).collect(),
        count,
        tag_style: crate::cell_text("", cx).meta(cx).style,
        count_style: body.color(ui::Color::Muted.color(cx)).style,
        background: crate::ds::surface::tag(cx),
        radius: px(crate::ds::radii(cx).xs),
    }
}

impl IntoElement for Tags {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

/// The lines, shaped as they are laid out, and their heights.
pub struct Shaped {
    tags: Vec<ShapedLine>,
    tag_line_height: Pixels,
    count: Option<Box<ShapedLine>>,
    count_line_height: Pixels,
}

/// Where each part is painted, in window pixels, and each tag's line as it fits.
pub struct Placed {
    clip: Bounds<Pixels>,
    tags: Vec<(Bounds<Pixels>, Point<Pixels>, ShapedLine)>,
    count: Option<Point<Pixels>>,
}

impl Element for Tags {
    type RequestLayoutState = Shaped;
    type PrepaintState = Placed;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    /// The whole of its parent's box, and no lower than a tag or the count's line. It centres
    /// its parts across that height itself: GPUI gives an element its bounds on device pixels,
    /// and boxes centred in a smaller element would be placed from where that element was
    /// before it was put on them, half a pixel off where the boxes were.
    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Shaped) {
        let (tags, tag_line_height) =
            window.with_text_style(Some(self.tag_style.clone()), |window| {
                let style = window.text_style();
                let font_size = style.font_size.to_pixels(window.rem_size());
                let lines = self
                    .tags
                    .iter()
                    .map(|tag| shape_line(tag, &style, font_size, window))
                    .collect();
                (lines, line_height(&style, font_size, window))
            });
        let (count, count_line_height) = match &self.count {
            Some(count) => window.with_text_style(Some(self.count_style.clone()), |window| {
                let style = window.text_style();
                let font_size = style.font_size.to_pixels(window.rem_size());
                (
                    Some(Box::new(shape_line(count, &style, font_size, window))),
                    line_height(&style, font_size, window),
                )
            }),
            None => (None, px(0.)),
        };
        let shaped = Shaped {
            tags,
            tag_line_height,
            count,
            count_line_height,
        };
        let mut style = Style::default();
        style.size.width = Length::Definite(DefiniteLength::Fraction(1.));
        style.min_size.width = Length::Definite(DefiniteLength::Absolute(px(0.).into()));
        style.size.height = Length::Definite(DefiniteLength::Fraction(1.));
        style.min_size.height = Length::Definite(DefiniteLength::Absolute(shaped.height().into()));
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
    ) -> Placed {
        let scale = window.scale_factor();
        let plan = plan(
            &shaped
                .tags
                .iter()
                .map(|line| hug_width(line.width, scale))
                .collect::<SmallVec<[f32; 2]>>(),
            shaped
                .count
                .as_ref()
                .map(|line| hug_width(line.width, scale)),
            f32::from(bounds.size.width),
            f32::from(shaped.height()),
            f32::from(shaped.tag_line_height),
            f32::from(shaped.count_line_height),
        );
        // Where the row of parts would stand, centred in the element as its parent centred it.
        let origin = bounds.origin.map(f32::from)
            + Point::new(0., f32::from(bounds.size.height - shaped.height()) / 2.);
        let snap = |rect: Rect| rect.snapped(origin, scale);
        let tags = std::mem::take(&mut shaped.tags)
            .into_iter()
            .zip(&self.tags)
            .zip(&plan.tags)
            .map(|((line, text), place)| {
                let text_bounds = snap(place.text);
                let line = if line.width <= text_bounds.size.width {
                    line
                } else {
                    window.with_text_style(Some(self.tag_style.clone()), |window| {
                        cut_line(text, text_bounds.size.width, window, cx)
                    })
                };
                (snap(place.tag), text_bounds.origin, line)
            })
            .collect();
        let placed = Placed {
            clip: snap(plan.clip),
            tags,
            count: plan.count.map(|count| snap(count).origin),
        };
        #[cfg(test)]
        tests::PLACED.with_borrow_mut(|all| {
            all.push((
                placed.clip,
                placed.tags.iter().map(|(tag, _, _)| *tag).collect(),
                placed.count,
            ))
        });
        placed
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        shaped: &mut Shaped,
        placed: &mut Placed,
        window: &mut Window,
        cx: &mut App,
    ) {
        if !placed.tags.is_empty() {
            let mask = gpui::ContentMask {
                bounds: placed.clip,
            };
            window.with_content_mask(Some(mask), |window| {
                let mut transparent = self.background;
                transparent.a = 0.;
                for (tag, text, line) in &placed.tags {
                    let radius = self
                        .radius
                        .min(tag.size.height / 2.)
                        .min(tag.size.width / 2.);
                    window.paint_quad(gpui::quad(
                        *tag,
                        radius,
                        self.background,
                        gpui::Edges::default(),
                        transparent,
                        gpui::BorderStyle::default(),
                    ));
                    line.paint(
                        *text,
                        shaped.tag_line_height,
                        TextAlign::Left,
                        None,
                        window,
                        cx,
                    )
                    .ok();
                }
            });
        }
        if let (Some(origin), Some(line)) = (placed.count, &shaped.count) {
            line.paint(
                origin,
                shaped.count_line_height,
                TextAlign::Left,
                None,
                window,
                cx,
            )
            .ok();
        }
    }
}

impl Shaped {
    /// As high as a tag when there is one, and as the count's line when there is one.
    fn height(&self) -> Pixels {
        let tag = if self.tags.is_empty() {
            px(0.)
        } else {
            px(size::TAG)
        };
        let count = if self.count.is_some() {
            self.count_line_height
        } else {
            px(0.)
        };
        tag.max(count)
    }
}

/// The width a hugging line asks for: its own, up to a whole device pixel, as
/// [`crate::CellText::hug`] asks.
fn hug_width(width: Pixels, scale: f32) -> f32 {
    (f32::from(width) * scale).ceil() / scale
}

/// A box relative to the element's origin, before it is put on device pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Rect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

impl Rect {
    /// On device pixels as GPUI puts a laid-out box: each edge in window space, a half rounded
    /// toward zero (`gpui::taffy::TaffyLayoutEngine::layout_bounds`).
    fn snapped(self, origin: Point<f32>, scale: f32) -> Bounds<Pixels> {
        let edge = |at: f32| (((at * scale).abs() - 0.5).ceil().copysign(at * scale)) / scale;
        let (left, top) = (edge(origin.x + self.x), edge(origin.y + self.y));
        let (right, bottom) = (
            edge(origin.x + self.x + self.w),
            edge(origin.y + self.y + self.h),
        );
        Bounds {
            origin: Point::new(px(left), px(top)),
            size: Size::new(px(right - left), px(bottom - top)),
        }
    }
}

/// One tag's box and its line's box.
#[derive(Debug, PartialEq)]
struct TagPlace {
    tag: Rect,
    text: Rect,
}

/// Where the parts go in a `width` by `height` element: the row the tags are clipped to, each
/// tag, and the count.
#[derive(Debug, PartialEq)]
struct Plan {
    clip: Rect,
    tags: SmallVec<[TagPlace; 2]>,
    count: Option<Rect>,
}

/// Lays the parts out as the boxes [`crate::ds::tag`] draws would be: a row of the tags, each
/// as wide as its line and its padding up to [`TAG_MAX_WIDTH`], a gap apart, that gives way to
/// the count after it (a gap further on, as wide as its line, never narrower), all centred
/// across the element. `tag_widths` and `count_width` are the lines' hugging widths.
fn plan(
    tag_widths: &[f32],
    count_width: Option<f32>,
    width: f32,
    height: f32,
    tag_line_height: f32,
    count_line_height: f32,
) -> Plan {
    let (gap, padding) = (space::XS, space::SM);
    let tag_height = if tag_widths.is_empty() { 0. } else { size::TAG };
    let boxes: SmallVec<[f32; 2]> = tag_widths
        .iter()
        .map(|text| (text + 2. * padding).min(TAG_MAX_WIDTH))
        .collect();
    let content = boxes.iter().sum::<f32>() + gap * boxes.len().saturating_sub(1) as f32;
    // The row of tags shrinks to what the count leaves; the count keeps its width.
    let room = match count_width {
        Some(count) => width - gap - count,
        None => width,
    };
    let row_width = content.min(room).max(0.);
    let clip = Rect {
        x: 0.,
        y: (height - tag_height) / 2.,
        w: row_width,
        h: tag_height,
    };
    let mut x = 0.;
    let tags = boxes
        .iter()
        .map(|&w| {
            let tag = Rect {
                x,
                y: clip.y,
                w,
                h: tag_height,
            };
            x += w + gap;
            TagPlace {
                tag,
                text: Rect {
                    x: tag.x + padding,
                    y: tag.y + (tag_height - tag_line_height) / 2.,
                    w: (w - 2. * padding).max(0.),
                    h: tag_line_height,
                },
            }
        })
        .collect();
    let count = count_width.map(|w| Rect {
        x: row_width + gap,
        y: (height - count_line_height) / 2.,
        w,
        h: count_line_height,
    });
    Plan { clip, tags, count }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};
    use std::cell::RefCell;

    type Drawn = (Bounds<Pixels>, Vec<Bounds<Pixels>>, Option<Point<Pixels>>);

    thread_local! {
        /// What each [`Tags`] drawn in a test placed: its clip, its tags and its count.
        pub(super) static PLACED: RefCell<Vec<Drawn>> = const { RefCell::new(Vec::new()) };
    }

    /// The same parts drawn twice, one under the other, in a cell's padded box: as the boxes
    /// the table drew before (a row of [`crate::ds::tag`]s, clipped, then the count) and as a
    /// [`Tags`].
    struct Probe {
        width: f32,
        height: f32,
        tags: Vec<&'static str>,
        count: Option<&'static str>,
    }

    impl gpui::Render for Probe {
        fn render(
            &mut self,
            _window: &mut Window,
            cx: &mut gpui::Context<Self>,
        ) -> impl IntoElement {
            use ui::prelude::*;
            let cell = || {
                div()
                    .flex_none()
                    .w(px(self.width))
                    .h(px(self.height))
                    .px(px(space::LG))
                    .flex()
                    .items_center()
            };
            let boxes = h_flex()
                .w_full()
                .min_w_0()
                .gap(px(space::XS))
                .child(
                    h_flex()
                        .debug_selector(|| "boxes-row".into())
                        .min_w_0()
                        .gap(px(space::XS))
                        .overflow_hidden()
                        .children(self.tags.iter().enumerate().map(|(ix, tag)| {
                            crate::ds::tag(*tag, cx).debug_selector(move || format!("box-{ix}"))
                        })),
                )
                .when_some(self.count, |row, count| {
                    row.child(
                        div()
                            .debug_selector(|| "box-count".into())
                            .flex_none()
                            .child(
                                crate::cell_text(count, cx)
                                    .color(Color::Muted.color(cx))
                                    .hug(),
                            ),
                    )
                });
            let element = tags(
                self.tags.iter().map(|tag| SharedString::from(*tag)),
                self.count.map(SharedString::from),
                cx,
            );
            v_flex()
                .child(cell().child(boxes))
                .child(cell().child(element))
        }
    }

    /// The element puts each tag, the row it is clipped to and the count on the bounds the
    /// boxes had, one cell lower: with room, short of room, past a tag's widest, with no tag,
    /// and in a cell whose height centres them on a half pixel.
    #[gpui::test]
    fn the_element_draws_where_the_boxes_did(cx: &mut TestAppContext) {
        cx.update(studio_theme::init_test);
        let cases = [
            (300., 32., vec!["Ada Lovelace", "Grace Hopper"], Some("+3")),
            (120., 32., vec!["Ada Lovelace", "Grace Hopper"], Some("+3")),
            (
                600.,
                33.,
                vec!["A name far too long to fit in one tag of a cell"],
                None,
            ),
            (200., 32., vec![], Some("12")),
            (160., 31., vec!["Ada"], None),
        ];
        for (width, height, names, count) in cases {
            PLACED.with_borrow_mut(Vec::clear);
            let (_, cx): (_, &mut VisualTestContext) = cx.add_window_view(|_, _| Probe {
                width,
                height,
                tags: names.clone(),
                count,
            });
            cx.run_until_parked();
            let (clip, drawn, count_at) = PLACED
                .with_borrow(|all| all.last().cloned())
                .expect("the element was drawn");
            let lower = |bounds: Bounds<Pixels>| Bounds {
                origin: bounds.origin + Point::new(px(0.), px(height)),
                ..bounds
            };
            let case = format!("{width}x{height} {names:?} {count:?}");
            for (ix, tag) in drawn.iter().enumerate() {
                let boxed = cx
                    .debug_bounds(format!("box-{ix}").leak())
                    .unwrap_or_else(|| panic!("{case}: box {ix} was not drawn"));
                assert_eq!(*tag, lower(boxed), "{case}: tag {ix}");
            }
            assert_eq!(drawn.len(), names.len(), "{case}");
            if !names.is_empty() {
                let row = cx.debug_bounds("boxes-row").expect("the row");
                assert_eq!(clip, lower(row), "{case}: the row the tags are clipped to");
            }
            match (count_at, cx.debug_bounds("box-count")) {
                (Some(at), Some(boxed)) => {
                    assert_eq!(at, lower(boxed).origin, "{case}: the count")
                }
                (None, None) => {}
                other => panic!("{case}: count {other:?}"),
            }
        }
    }

    /// Two tags and a count, with room for all: the tags a gap apart, padded, the count a gap
    /// after them, everything centred in the row's height.
    #[test]
    fn tags_and_a_count_sit_as_the_boxes_would() {
        let plan = plan(&[30., 40.], Some(15.), 200., 18., 14., 16.);
        assert_eq!(plan.tags.len(), 2);
        assert_eq!(
            plan.tags[0].tag,
            Rect {
                x: 0.,
                y: 0.,
                w: 42.,
                h: 18.
            }
        );
        assert_eq!(
            plan.tags[0].text,
            Rect {
                x: 6.,
                y: 2.,
                w: 30.,
                h: 14.
            }
        );
        assert_eq!(plan.tags[1].tag.x, 46.);
        assert_eq!(plan.tags[1].tag.w, 52.);
        assert_eq!(plan.clip.w, 98.);
        assert_eq!(
            plan.count,
            Some(Rect {
                x: 102.,
                y: 1.,
                w: 15.,
                h: 16.
            })
        );
    }

    /// A long name stops at the widest a tag grows, and its line gets what is left inside.
    #[test]
    fn a_long_tag_stops_at_its_widest() {
        let plan = plan(&[400.], None, 600., 18., 14., 0.);
        assert_eq!(plan.tags[0].tag.w, TAG_MAX_WIDTH);
        assert_eq!(plan.tags[0].text.w, TAG_MAX_WIDTH - 2. * space::SM);
        assert_eq!(plan.count, None);
    }

    /// Short of room, the row of tags gives way and the count keeps its width at the row's end.
    #[test]
    fn the_tags_give_way_to_the_count() {
        let short = plan(&[60., 60.], Some(20.), 100., 18., 14., 16.);
        assert_eq!(short.clip.w, 100. - space::XS - 20.);
        assert_eq!(short.count.unwrap().x, 100. - 20.);
        // No tags: the count still stands a gap in, where the empty row of tags ends.
        let bare = plan(&[], Some(20.), 100., 16., 14., 16.);
        assert_eq!(bare.count.unwrap().x, space::XS);
        assert_eq!(bare.clip.h, 0.);
    }

    /// Edges land on device pixels as GPUI's layout puts them, a half toward zero.
    #[test]
    fn edges_land_on_device_pixels() {
        let rect = Rect {
            x: 0.5,
            y: 1.5,
            w: 10.,
            h: 3.,
        };
        let at = rect.snapped(Point::new(10., 20.), 1.);
        assert_eq!(at.origin, Point::new(px(10.), px(21.)));
        assert_eq!(at.size, Size::new(px(10.), px(3.)));
        let at = rect.snapped(Point::new(10., 20.), 2.);
        assert_eq!(at.origin, Point::new(px(10.5), px(21.5)));
    }

    /// GPUI moves an element and its states from step to step of every frame, for each cell.
    #[test]
    fn a_cell_moves_small_states() {
        assert!(std::mem::size_of::<Shaped>() <= 64);
        assert!(std::mem::size_of::<Placed>() <= 64);
    }
}
