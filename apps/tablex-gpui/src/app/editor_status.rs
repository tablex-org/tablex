//! The thin status line under a query editor (SQL, Redis console, MongoDB script, aggregation
//! pipeline): on the left the Vim switch, the Vim mode while Vim is on and the `.sql` file the tab
//! is bound to (with a dot while it has unsaved changes); on the right the selection, `Ln X, Col Y`
//! and the language.
//!
//! It is built by the pane that renders the editor, so it follows the editor with no state of its
//! own: reading the editor's state while rendering makes the pane redraw when the cursor, the
//! selection or the text change, and nothing else does. Line, column and the selection's size come
//! from the rope in O(log n) (no scan of the text, nothing is copied).

use super::*;
use crate::vim::Mode;
use gpui_component::input::{Rope, RopeExt as _};

/// What the status line calls the language of an editor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum EditorLanguage {
    Sql,
    MongoScript,
    Redis,
    Aggregation,
}

impl EditorLanguage {
    /// The language of the editor of a tab in `mode`.
    pub(super) fn of(mode: Option<&QueryMode>) -> Self {
        if mode.is_mongo_script() {
            Self::MongoScript
        } else if mode.is_redis_command() {
            Self::Redis
        } else if mode.is_mongo_aggregate() {
            Self::Aggregation
        } else {
            Self::Sql
        }
    }

    fn label(self) -> String {
        match self {
            Self::Sql => t!("editor.language_sql"),
            Self::MongoScript => t!("editor.language_mongo_script"),
            Self::Redis => t!("editor.language_redis"),
            Self::Aggregation => t!("editor.language_aggregation"),
        }
        .to_string()
    }
}

/// How much of the text is selected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SelectedText {
    pub(super) lines: usize,
    pub(super) chars: usize,
}

/// Where the cursor is (1-based line, column in characters) and what is selected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CursorInfo {
    pub(super) line: usize,
    pub(super) column: usize,
    pub(super) selected: Option<SelectedText>,
}

/// The cursor and selection of an editor holding `text`. `cursor` is the moving end of
/// `selection`. O(log n): the column is the character length of the cursor's line up to the cursor.
pub(super) fn cursor_info(text: &Rope, selection: &Range<usize>, cursor: usize) -> CursorInfo {
    let point = text.offset_to_point(cursor);
    let line_start = text.line_start_offset(point.row);
    let column = text.slice(line_start..line_start + point.column).len_chars();
    let selected = (!selection.is_empty()).then(|| {
        let (start, end) = (text.offset_to_point(selection.start), text.offset_to_point(selection.end));
        let mut lines = end.row - start.row + 1;
        // A selection that ends at the start of a line does not touch that line.
        if end.row > start.row && end.column == 0 {
            lines -= 1;
        }
        SelectedText { lines, chars: text.slice(selection.clone()).len_chars() }
    });
    CursorInfo { line: point.row + 1, column: column + 1, selected }
}

/// The background of the code area, which the component keeps to itself: the highlight theme's, else
/// the input background. The status line and the box around the editor use it to look like one piece.
pub(super) fn editor_background(cx: &gpui::App) -> gpui::Hsla {
    let theme = gpui_component::theme::Theme::global(cx);
    theme.highlight_theme.style.editor_background.unwrap_or_else(|| theme.input_background())
}

/// The accent color of a Vim mode: each mode its own.
pub(super) fn mode_color(mode: Mode) -> gpui::Rgba {
    match mode {
        Mode::Normal => theme::accent(),
        Mode::Insert => theme::success(),
        Mode::Visual => theme::warning(),
        Mode::VisualLine => theme::icon_view(),
    }
}

fn selection_label(selected: SelectedText) -> String {
    match (selected.lines, selected.chars) {
        (1, 1) => t!("editor.status_selected_one").to_string(),
        (1, chars) => t!("editor.status_selected", chars = chars).to_string(),
        (lines, chars) => t!("editor.status_selected_lines", lines = lines, chars = chars).to_string(),
    }
}

/// The texts of a status line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct StatusText {
    pub(super) position: String,
    pub(super) selection: Option<String>,
    /// The file the tab is bound to, with a dot while it has unsaved changes.
    pub(super) file: Option<String>,
    pub(super) language: String,
}

impl AppRoot {
    /// What the status line of `editor` says now (reads the editor, so a pane that renders it
    /// redraws when the cursor, the selection or the text change).
    pub(super) fn editor_status_text(
        &self,
        tab_id: u64,
        editor: &gpui::Entity<EditorState>,
        language: EditorLanguage,
        cx: &gpui::App,
    ) -> StatusText {
        let info = {
            let state = editor.read(cx);
            cursor_info(state.text(), &state.selected_range(), state.cursor())
        };
        let file = self.workspace.tab(tab_id).and_then(|tab| {
            let file = tab.file.as_ref()?;
            let name = file.path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
            Some(if tab.has_unsaved_file() { format!("{name} \u{25CF}") } else { name })
        });
        StatusText {
            position: t!("editor.status_position", line = info.line, column = info.column).to_string(),
            selection: info.selected.map(selection_label),
            file,
            language: language.label(),
        }
    }

    /// The status line of `editor`; `vim` is whether the editor has Vim keys (and so the switch).
    pub(super) fn render_editor_status(
        &self,
        tab_id: u64,
        editor: &gpui::Entity<EditorState>,
        language: EditorLanguage,
        vim: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let StatusText { position, selection, file, language } = self.editor_status_text(tab_id, editor, language, cx);
        // Numbers in the editor's monospace font, so they do not jitter as the digits change.
        let mono = gpui_component::theme::Theme::global(cx).mono_font_family.clone();
        let label = |text: String, selector: &'static str| {
            div().debug_selector(move || selector.into()).flex_shrink_0().whitespace_nowrap().font_family(mono.clone()).child(text)
        };
        // The selection's words are in the UI font and only its numbers in the monospace one.
        let selection_label = |text: String| {
            div().debug_selector(|| "editor-status-selection".into()).flex_shrink_0().child(crate::ui::numbers_mono(&text, &mono))
        };
        // Part of the editor box: the editor's own background, a single faint hairline above.
        div()
            .debug_selector(|| "editor-status".into())
            .h(px(theme::EDITOR_STATUS_HEIGHT))
            .flex_shrink_0()
            .w_full()
            .pl(px(6.0))
            .pr(px(10.0))
            .flex()
            .items_center()
            .gap(px(10.0))
            .overflow_hidden()
            .bg(editor_background(cx))
            .border_t_1()
            .border_color(gpui::Hsla::from(theme::border()).opacity(0.6))
            .text_xs()
            .text_color(theme::text_muted())
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .items_center()
                    .gap(px(8.0))
                    .overflow_hidden()
                    .when(vim, |this| this.child(self.render_vim_controls(tab_id, cx)))
                    .when_some(file, |this, file| {
                        this.child(
                            div()
                                .debug_selector(|| "editor-status-file".into())
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .child(file),
                        )
                    }),
            )
            .children(selection.map(selection_label))
            .child(label(position, "editor-status-position"))
            // Not monospace: it is a name, and the one item that is not a number.
            .child(div().debug_selector(|| "editor-status-language".into()).flex_shrink_0().whitespace_nowrap().child(language))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(text: &str, selection: Range<usize>, cursor: usize) -> CursorInfo {
        cursor_info(&Rope::from_str(text), &selection, cursor)
    }

    #[test]
    fn the_position_is_one_based_and_counts_characters() {
        assert_eq!((info("", 0..0, 0).line, info("", 0..0, 0).column), (1, 1));
        let text = "select 1\nfrom \u{e9}t\u{e9}\nx";
        let at = |offset| {
            let info = info(text, offset..offset, offset);
            (info.line, info.column)
        };
        assert_eq!(at(0), (1, 1));
        assert_eq!(at(8), (1, 9), "the end of a line");
        assert_eq!(at(9), (2, 1), "after the newline");
        // `e` with an accent is two bytes but one column.
        assert_eq!(at(9 + "from \u{e9}".len()), (2, 7));
        assert_eq!(at(text.len()), (3, 2));
    }

    #[test]
    fn the_selection_counts_characters_and_the_lines_it_touches() {
        let text = "one\ntwo\nthree";
        assert_eq!(info(text, 1..1, 1).selected, None);
        assert_eq!(info(text, 0..3, 3).selected, Some(SelectedText { lines: 1, chars: 3 }));
        assert_eq!(info(text, 1..6, 6).selected, Some(SelectedText { lines: 2, chars: 5 }));
        assert_eq!(info(text, 0..13, 13).selected, Some(SelectedText { lines: 3, chars: 13 }));
        // Ending at the start of a line does not touch it (Visual-line selections do this).
        assert_eq!(info(text, 0..8, 8).selected, Some(SelectedText { lines: 2, chars: 8 }));
        assert_eq!(info("\u{e9}\u{e9}", 0..4, 4).selected, Some(SelectedText { lines: 1, chars: 2 }));
    }

    #[test]
    fn each_vim_mode_has_its_own_color() {
        let modes = [Mode::Normal, Mode::Insert, Mode::Visual, Mode::VisualLine];
        for (index, a) in modes.iter().enumerate() {
            for b in &modes[index + 1..] {
                assert_ne!(mode_color(*a), mode_color(*b), "{a:?} and {b:?}");
            }
        }
    }
}
