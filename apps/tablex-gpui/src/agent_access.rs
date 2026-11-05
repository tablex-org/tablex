//! The "Agent access" choice shared by the connection dialog (a connection's own setting) and the
//! Settings page (a connection type's default): the levels as `Select` items, each with its
//! one-line explanation in the dropdown. The levels, their ids and the policy live in
//! `engine-connect::profile` (`engine-agent` computes the effective level).

use crate::theme;
use engine_connect::profile::AgentLevel;
use gpui::{div, px, App, AppContext, Entity, IntoElement, ParentElement, SharedString, Styled, Window};
use gpui_component::select::{SelectItem, SelectState};
use gpui_component::IndexPath;

/// One level as a `Select` row: the label alone in the closed control, label and explanation in the list.
#[derive(Clone)]
pub struct AgentChoice {
    level: AgentLevel,
}

impl AgentChoice {
    pub fn list(levels: &[AgentLevel]) -> Vec<Self> {
        levels.iter().map(|level| Self { level: *level }).collect()
    }
}

impl SelectItem for AgentChoice {
    type Value = AgentLevel;

    fn title(&self) -> SharedString {
        self.level.label().into()
    }

    fn render(&self, _: &mut Window, _: &mut App) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap(px(1.0))
            .child(div().text_size(px(13.0)).text_color(theme::text()).child(self.level.label()))
            .child(div().text_size(px(11.0)).text_color(theme::text_muted()).child(self.level.description()))
    }

    fn value(&self) -> &AgentLevel {
        &self.level
    }
}

/// A `Select` over `levels` showing `selected` (`None`: nothing chosen, e.g. a hand-edited level
/// the list does not offer).
pub fn agent_select(
    levels: &[AgentLevel],
    selected: Option<AgentLevel>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<SelectState<Vec<AgentChoice>>> {
    let row = selected.and_then(|level| levels.iter().position(|offered| *offered == level));
    cx.new(|cx| SelectState::new(AgentChoice::list(levels), row.map(|row| IndexPath::default().row(row)), window, cx))
}
