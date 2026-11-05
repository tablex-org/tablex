use gpui::{div, px, Div, ParentElement, Rgba, Styled};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::{Icon, IconName};

use crate::theme;

pub fn icon_button(
    id: impl Into<gpui::ElementId>,
    icon: IconName,
    tooltip: &'static str,
) -> Button {
    Button::new(id)
        .icon(Icon::new(icon).size(px(14.0)))
        .compact()
        .ghost()
        .tooltip(tooltip)
}

pub fn action_button(id_prefix: &'static str, tab_id: u64, label: &'static str) -> Button {
    Button::new((id_prefix, tab_id))
        .label(label)
        .compact()
        .ghost()
}

pub fn primary_action_button(id_prefix: &'static str, tab_id: u64, label: &'static str) -> Button {
    Button::new((id_prefix, tab_id))
        .label(label)
        .compact()
        .primary()
}

pub fn separator() -> Div {
    div().w(px(1.0)).h(px(18.0)).bg(theme::border())
}

pub fn status_dot(color: Rgba) -> Div {
    div().size(px(6.0)).rounded(px(3.0)).bg(color)
}

pub fn section_header(title: impl Into<String>) -> Div {
    div()
        .flex()
        .items_center()
        .h(px(30.0))
        .px(px(12.0))
        .text_size(px(11.0))
        .text_color(theme::text_muted())
        .child(title.into())
}
