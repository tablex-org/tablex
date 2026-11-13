//! The text of one Tree view row (`key: value`) as a participant of the
//! window-level text selection, so a drag can select across rows and Cmd+C
//! copies what is highlighted.
//!
//! `gpui_base::SelectableText` is the component for this, but it paints one
//! style for the whole run and the key must keep its own colour. This is the
//! same element with one colour range added; it uses only the public
//! `TextSelectionHandle` API. The cost per visible row is that of
//! `SelectableText`: one retained handle.

use gpui::{
    App, BorderStyle, Bounds, Corners, Edges, Element, ElementId, GlobalElementId, HighlightStyle,
    Hitbox, HitboxBehavior, Hsla, InspectorElementId, IntoElement, LayoutId, PaintQuad, Pixels,
    SharedString, StyledText, Window, transparent_black,
};
use gpui_base::{TextSelectionHandle, TextSelectionRegistration, TextSelectionRun};
use std::ops::Range;

pub(super) struct TreeRowText {
    id: ElementId,
    text: SharedString,
    styled_text: StyledText,
    document_order: u64,
}

impl TreeRowText {
    /// `text` is the whole line; `key` is the byte range painted in `key_color`.
    /// `document_order` is the row's position, which orders the copied lines.
    pub(super) fn new(
        id: impl Into<ElementId>,
        text: String,
        key: Range<usize>,
        key_color: Hsla,
        document_order: u64,
    ) -> Self {
        let text = SharedString::from(text);
        let styled_text = if key.is_empty() {
            StyledText::new(text.clone())
        } else {
            StyledText::new(text.clone()).with_highlights([(key, HighlightStyle::color(key_color))])
        };
        Self { id: id.into(), text, styled_text, document_order }
    }
}

impl IntoElement for TreeRowText {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TreeRowText {
    type RequestLayoutState = TextSelectionHandle;
    type PrepaintState = Hitbox;

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        // The view drawing this row: the result surface, a cached view. A drag only emits
        // `SelectionChanged`; a window refresh would replay the cached frame, so the view itself
        // is notified, and the highlight follows the pointer while the button is held.
        let view = window.current_view();
        let handle = window.with_element_state(
            global_id.expect("a row text has an id"),
            |retained: Option<(TextSelectionHandle, gpui::Subscription)>, _| {
                let (handle, subscription) = retained.unwrap_or_else(|| {
                    let handle = TextSelectionHandle::new(self.text.clone(), cx);
                    let subscription = handle.subscribe(
                        move |event, cx| {
                            if matches!(event, gpui_base::TextSelectionEvent::SelectionChanged(_)) {
                                cx.notify(view);
                            }
                        },
                        cx,
                    );
                    (handle, subscription)
                });
                (handle.clone(), (handle, subscription))
            },
        );
        let (layout_id, ()) = self.styled_text.request_layout(global_id, inspector_id, window, cx);
        (layout_id, handle)
    }

    fn prepaint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        handle: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.styled_text.prepaint(global_id, inspector_id, bounds, &mut (), window, cx);
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        let registration = TextSelectionRegistration::new(hitbox.clone(), bounds)
            .with_document_order(self.document_order)
            .with_text_bounds(vec![bounds])
            .with_rendered_element(handle, window, cx);
        handle.register(registration, window, cx);
        hitbox
    }

    fn paint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        handle: &mut Self::RequestLayoutState,
        _: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let layout = self.styled_text.layout().clone();
        let projection = handle.update_runs(
            &[TextSelectionRun::new(self.text.clone(), layout.clone(), bounds)
                .with_document_order(self.document_order)],
            cx,
        );
        // The editor's selection colour, as in the JSON view.
        let color = gpui_component::ActiveTheme::theme(cx).selection;
        for range in projection.ranges().iter().flatten() {
            // One line: the text never wraps in a row.
            if let (Some(start), Some(end)) =
                (layout.position_for_index(range.start), layout.position_for_index(range.end))
            {
                window.paint_quad(PaintQuad {
                    bounds: Bounds::from_corners(
                        start,
                        gpui::point(end.x, end.y + layout.line_height()),
                    ),
                    background: color.into(),
                    corner_radii: Corners::default(),
                    border_widths: Edges::default(),
                    border_color: transparent_black(),
                    border_style: BorderStyle::default(),
                });
            }
        }
        self.styled_text.paint(global_id, inspector_id, bounds, &mut (), &mut (), window, cx);
    }
}
