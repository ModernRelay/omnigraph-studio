//! Drawing an element over a place another view drew, so the element can change without that
//! view being drawn again.
//!
//! GPUI draws every view around a changed element again, and a cached view inside a view that
//! draws again is drawn again too (`prepaint_view`). A field drawn inside a large view, such as
//! a table's grid, would make that view lay out all its rows on every key. Instead the large
//! view marks the place with [`OverlaySlot::mark`] while it draws, and an element after it in
//! the tree, outside it, draws the field there with [`OverlaySlot::overlay`], clipped to the
//! part of the place that showed.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    AnyElement, App, AvailableSpace, Bounds, ContentMask, Element, ElementId, GlobalElementId,
    InspectorElementId, IntoElement, LayoutId, Pixels, Position, Size, Style, Styled as _, Window,
    canvas,
};

/// Where the place is in the window, and the part of the window it showed in.
type Place = Rc<Cell<Option<(Bounds<Pixels>, ContentMask<Pixels>)>>>;

/// A place one element marks and another draws over. Cloning shares the place.
#[derive(Clone, Default)]
pub struct OverlaySlot(Place);

impl OverlaySlot {
    /// Forgets the place. The marking view calls this when it starts to draw, so a place it
    /// no longer draws (a row scrolled away) is not used.
    pub fn clear(&self) {
        self.0.set(None);
    }

    /// An element that fills its parent and records where it is and how much of it shows.
    pub fn mark(&self) -> impl IntoElement {
        let place = self.0.clone();
        canvas(
            move |bounds, window, _| place.set(Some((bounds, window.content_mask()))),
            |_, _, _, _| {},
        )
        .size_full()
    }

    /// `child` drawn over the marked place at its size, or nothing while no place is marked.
    /// It must come after the marking view in the tree, so the place is known when it is laid
    /// out.
    pub fn overlay(&self, child: impl IntoElement) -> OverlaySlotElement {
        OverlaySlotElement {
            place: self.0.clone(),
            child: child.into_any_element(),
        }
    }
}

pub struct OverlaySlotElement {
    place: Place,
    child: AnyElement,
}

impl IntoElement for OverlaySlotElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for OverlaySlotElement {
    type RequestLayoutState = ();
    /// The visible part of the place, when the child was laid out there.
    type PrepaintState = Option<ContentMask<Pixels>>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    /// Takes no room where it stands: the child is placed when the marked place is known.
    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let style = Style {
            position: Position::Absolute,
            ..Style::default()
        };
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Option<ContentMask<Pixels>> {
        let (bounds, mask) = self.place.get()?;
        window.with_content_mask(Some(mask), |window| {
            self.child
                .layout_as_root(Size::<AvailableSpace>::from(bounds.size), window, cx);
            self.child.prepaint_at(bounds.origin, window, cx);
        });
        Some(mask)
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        mask: &mut Option<ContentMask<Pixels>>,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let Some(mask) = *mask {
            window.with_content_mask(Some(mask), |window| self.child.paint(window, cx));
        }
    }
}
