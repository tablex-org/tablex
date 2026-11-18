use gpui::{div, px, Div, ParentElement, Rgba, Styled};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::{Icon, IconName};

use crate::theme;

pub fn icon_button(
    id: impl Into<gpui::ElementId>,
    icon: IconName,
    tooltip: impl Into<gpui::SharedString>,
) -> Button {
    Button::new(id)
        .icon(Icon::new(icon).size(px(14.0)))
        .compact()
        .ghost()
        .tooltip(tooltip)
}

pub fn action_button(id_prefix: &'static str, tab_id: u64, label: impl Into<gpui::SharedString>) -> Button {
    Button::new((id_prefix, tab_id))
        .label(label)
        .compact()
        .ghost()
}

pub fn primary_action_button(id_prefix: &'static str, tab_id: u64, label: impl Into<gpui::SharedString>) -> Button {
    Button::new((id_prefix, tab_id))
        .label(label)
        .compact()
        .primary()
}

pub fn separator() -> Div {
    div().w(px(1.0)).h(px(18.0)).bg(theme::border())
}

/// `text` cut into runs, each marked whether it is a number: digits, and a `+` right after them.
fn number_runs(text: &str) -> Vec<(&str, bool)> {
    let mut runs: Vec<(&str, bool)> = Vec::new();
    let (mut start, mut kind, mut previous_digit) = (0, false, false);
    for (at, character) in text.char_indices() {
        let number = character.is_ascii_digit() || (character == '+' && previous_digit);
        previous_digit = character.is_ascii_digit();
        if at > start && number != kind {
            runs.push((&text[start..at], kind));
            start = at;
        }
        kind = number;
    }
    if start < text.len() {
        runs.push((&text[start..], kind));
    }
    runs
}

/// A line of text with only its numbers in `mono` (so they do not jitter as they change) and the rest in
/// the inherited UI font. Does not wrap.
pub fn numbers_mono(text: &str, mono: &gpui::SharedString) -> Div {
    number_runs(text).into_iter().fold(div().flex().flex_shrink_0().whitespace_nowrap(), |line, (run, number)| {
        line.child(if number { div().font_family(mono.clone()).child(run.to_string()) } else { div().child(run.to_string()) })
    })
}

pub fn status_dot(color: Rgba) -> Div {
    div().size(px(6.0)).rounded(px(3.0)).bg(color)
}

pub fn section_header(title: impl ToString) -> Div {
    div()
        .flex()
        .items_center()
        .h(px(30.0))
        .px(px(12.0))
        .text_size(px(11.0))
        .text_color(theme::text_muted())
        .child(title.to_string())
}

#[cfg(test)]
mod tests {
    use super::number_runs;

    #[test]
    fn only_the_numbers_of_a_text_are_marked() {
        assert_eq!(number_runs("Selected: 16 cells \u{b7} 4 rows"), [("Selected: ", false), ("16", true), (" cells \u{b7} ", false), ("4", true), (" rows", false)]);
        assert_eq!(number_runs("200+ rows"), [("200+", true), (" rows", false)]);
        assert_eq!(number_runs("a + b"), [("a + b", false)]);
        assert_eq!(number_runs(""), []);
    }
}
