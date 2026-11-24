//! The Vim block caret: in Normal and Visual mode the editor shows a box one char wide over the
//! char under the caret instead of the thin bar, as Vim does.
//!
//! gpui-base's editor has no caret-shape API (its bar width is a private constant), so this is an
//! overlay drawn after the editor, in the same box. It reads `prepaint_caret_geometry`, which the
//! editor stores in its own prepaint: this element is a later sibling, so its prepaint runs after
//! the editor's and before anything is painted, and the box lands in the editor's frame (scrolling,
//! a moved caret and a resize included) with no frame of lag. The box is a one-char `div` laid out
//! in the editor's font, so its width is that char's advance (a wide char gets its two cells, the
//! end of a line one space). It is solid in the caret color with the char redrawn in the editor
//! background, which also covers the editor's thin caret under it; an unfocused editor gets an
//! outline. One element per editor, no entity, timer or polling; each frame costs one 1-char layout.
//!
//! Not drawn: Visual-line mode (the selected lines are the feedback) and, in Visual mode, a head on
//! an empty line (its position is not known to the editor).
use super::*;
use crate::vim::Mode;
use gpui::{
    AnyElement, App, AvailableSpace, Bounds, ContentMask, Element, ElementId, GlobalElementId, Hsla,
    InspectorElementId, IntoElement, LayoutId, Pixels, Position, Style,
};
use gpui_component::input::{Rope, RopeExt};

/// The colors and font of the box.
pub(super) struct BlockCaret {
    editor: gpui::Entity<EditorState>,
    mode: Mode,
    font_family: gpui::SharedString,
    font_size: Pixels,
    /// The box (and the outline of an unfocused editor).
    caret: Hsla,
    /// The editor's background: the color of the char inside the box.
    background: Hsla,
}

impl BlockCaret {
    /// The overlay for `editor` in Normal or Visual mode.
    pub(super) fn new(editor: gpui::Entity<EditorState>, mode: Mode, cx: &App) -> Self {
        let theme = gpui_component::theme::Theme::global(cx);
        Self {
            editor,
            mode,
            font_family: theme.mono_font_family.clone(),
            font_size: theme.mono_font_size,
            caret: theme.caret,
            background: editor_status::editor_background(cx),
        }
    }

    /// The char the box covers and how far left of the editor's caret it starts: in Normal mode the
    /// char at the caret; in Visual mode the selection's head, which is the char before the editor's
    /// caret when the selection runs forward. A line end is a space.
    fn covered(&self, text: &Rope, selection: std::ops::Range<usize>, reversed: bool) -> Option<(char, bool)> {
        let glyph = |ch: Option<char>| ch.filter(|ch| !ch.is_control()).unwrap_or(' ');
        match self.mode {
            Mode::Normal if selection.is_empty() => Some((glyph(text.char_at(selection.start)), false)),
            Mode::Visual if reversed => Some((glyph(text.char_at(selection.start)), false)),
            Mode::Visual if !selection.is_empty() => {
                let head = text.chars_at(selection.end).reversed().next()?;
                (head != '\n').then(|| (glyph(Some(head)), true))
            }
            _ => None,
        }
    }
}

impl Element for BlockCaret {
    type RequestLayoutState = ();
    /// The box, laid out at its place, and the editor's text area that clips it.
    type PrepaintState = Option<(AnyElement, Bounds<Pixels>)>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        // Out of the flow and empty: the box is placed in prepaint.
        let style = Style { position: Position::Absolute, ..Style::default() };
        (window.request_layout(style, [], cx), ())
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
        let state = self.editor.read(cx);
        // The editor's prepaint of this very frame.
        let geometry = state.prepaint_caret_geometry()?;
        let (glyph, before) = self.covered(state.text(), state.selected_range(), state.is_selection_reversed())?;
        let focused = state.focus_handle(cx).is_focused(window);
        let line_height = geometry.line_height();
        let caret = geometry.cursor_bounds();
        let mut cell = div()
            .debug_selector(|| "vim-block-caret".into())
            .flex()
            .flex_none()
            .whitespace_nowrap()
            .h(line_height)
            .line_height(line_height)
            .font_family(self.font_family.clone())
            .text_size(self.font_size)
            .when(focused, |this| this.bg(self.caret).text_color(self.background))
            // Hollow: the editor's own glyph shows through; a second one, shifted by the border,
            // would smear it.
            .when(!focused, |this| {
                this.border_1().border_color(self.caret).text_color(gpui::transparent_black())
            })
            // A box left of the caret also covers the editor's bar at its right edge.
            .when(before, |this| this.pr(caret.size.width))
            .child(glyph.to_string())
            .into_any_element();
        let size = cell.layout_as_root(
            gpui::Size { width: AvailableSpace::MaxContent, height: AvailableSpace::Definite(line_height) },
            window,
            cx,
        );
        // The editor's caret is a bar centred on its row; the box is the whole row.
        let centre = caret.origin.y + geometry.scroll_offset().y + caret.size.height / 2.0;
        let x = if before { caret.origin.x + caret.size.width - size.width } else { caret.origin.x };
        let area = geometry.input_bounds();
        window.with_content_mask(Some(ContentMask { bounds: area }), |window| {
            cell.prepaint_at(gpui::point(x, centre - line_height / 2.0), window, cx);
        });
        Some((cell, area))
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let Some((cell, area)) = prepaint {
            window.with_content_mask(Some(ContentMask { bounds: *area }), |window| cell.paint(window, cx));
        }
    }
}
