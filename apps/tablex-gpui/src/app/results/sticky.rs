//! Pinned grid columns stay at the left edge of the viewport while the grid
//! scrolls horizontally.
//!
//! GPUI 0.3.9 has no sticky positioning, and gpui-component's `DataTable` (the
//! only fixed-column implementation in the pinned components) is a different
//! table with its own scrolling and row model, not the virtualized grid this
//! app is built on. So the one thing missing is this element: it lays out and
//! paints its child exactly where it sits in the scrolled content, but shifts
//! the child's prepaint (hitboxes included) and paint by the horizontal scroll
//! offset *of the frame being drawn*, which cancels the scroll translation. No
//! state, no listeners, no per-frame allocation: a pointer copy and one offset
//! push per row, and the pinned cells are never one frame behind the scroll.

use gpui::{
    AnyElement, App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, Pixels, ScrollHandle, Window,
};

/// `child` follows the horizontal scroll of `scroll` instead of moving with it.
pub(in crate::app) struct StickyLeft {
    scroll: ScrollHandle,
    child: AnyElement,
}

impl StickyLeft {
    pub(in crate::app) fn new(scroll: &ScrollHandle, child: impl IntoElement) -> Self {
        Self {
            scroll: scroll.clone(),
            child: child.into_any_element(),
        }
    }
}

impl IntoElement for StickyLeft {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for StickyLeft {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        // The scroll offset is negative while scrolled right; the enclosing scroll
        // container has already pushed it, so pushing its negation puts the child
        // back at the viewport edge.
        let shift = gpui::point(-self.scroll.offset().x, gpui::px(0.0));
        window.with_element_offset(shift, |window| self.child.prepaint(window, cx));
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.paint(window, cx);
    }
}
