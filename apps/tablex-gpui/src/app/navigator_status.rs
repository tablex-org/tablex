//! Connection status and failure markers of navigator rows.
//!
//! * A connection that is live or connecting shows a small dot on the bottom
//!   right of its brand icon: green for connected, amber (a
//!   gentle pulse) for connecting, nothing for disconnected. The dot sits in a
//!   ring that has the row's own fill, so it reads on plain, tinted, selected
//!   and hovered rows.
//! * A connection that failed or was lost, and any node whose load failed, shows a
//!   warning triangle at the right edge of its row. Hovering it shows the first
//!   line of the error; clicking opens an official `Popover` with the whole
//!   text (selectable, scrollable) and Retry / Copy / Clear.
//!
//! Cost: a row without a status or failure adds one wrapper `div` around its
//! icon and nothing else. Only the open popover is held in app state
//! (`NavigatorUiState::problem_open`); its content is built while it is open.

use super::*;
use crate::navigator::NodeLoadState;
use gpui::{Animation, AnimationExt as _, ClipboardItem, Rgba, SharedString, WeakEntity};
use gpui_component::text::TextView;
use gpui_component::tooltip::Tooltip;
use std::rc::Rc;

/// Group name every navigator row shares, so a ring can follow the row's hover fill.
pub(super) const ROW_GROUP: &str = "nav-row";
/// Longest tooltip, in characters.
const SUMMARY_MAX_CHARS: usize = 120;
const DOT_SIZE: f32 = 6.0;
const RING_WIDTH: f32 = 1.5;
/// Cap of the connecting pulse, so a connecting row costs 12 redraws a second.
const PULSE_FPS: f32 = 12.0;

/// What a row's failure marker acts on.
pub(super) struct RowProblem {
    pub(super) message: SharedString,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProblemAction {
    Retry,
    Copy,
    Clear,
}

/// The failure of a row: a connection that failed or was lost (`status`), else
/// a node whose children failed to load. A disabled row shows none.
pub(super) fn row_problem(row: &NavigatorRow, status: &ConnectionStatus) -> Option<RowProblem> {
    if row.disabled {
        return None;
    }
    let message = match (status, &row.load) {
        (ConnectionStatus::Failed(message), _) => message,
        (_, Some(NodeLoadState::Failed(message))) => message,
        _ => return None,
    };
    Some(RowProblem {
        message: message.clone().into(),
    })
}

/// The first non-empty line of `message`, cut to a tooltip's length.
pub(super) fn problem_summary(message: &str) -> SharedString {
    let line = message
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    match line.char_indices().nth(SUMMARY_MAX_CHARS) {
        Some((end, _)) => format!("{}…", line[..end].trim_end()).into(),
        None => line.to_string().into(),
    }
}

/// `message` as Markdown that renders it verbatim: every ASCII punctuation
/// character escaped (CommonMark allows it), one hard-broken line per line.
pub(super) fn problem_markdown(message: &str) -> String {
    let mut out = String::with_capacity(message.len() + 16);
    for line in message.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if !out.is_empty() {
            out.push_str("  \n");
        }
        for ch in line.chars() {
            if ch.is_ascii_punctuation() {
                out.push('\\');
            }
            out.push(ch);
        }
    }
    out
}

/// `color` composited over the sidebar fill: the opaque colour a row paints.
pub(super) fn over_sidebar(color: Rgba) -> Rgba {
    let base = theme::sidebar();
    let mix = |below: f32, above: f32| below * (1.0 - color.a) + above * color.a;
    Rgba {
        r: mix(base.r, color.r),
        g: mix(base.g, color.g),
        b: mix(base.b, color.b),
        a: 1.0,
    }
}

/// The brand `icon` of a connection row with its status dot, when it has one.
/// `ring` / `ring_hover` are the row's opaque fill at rest and under the mouse.
pub(super) fn icon_with_status(
    icon: AnyElement,
    status: &ConnectionStatus,
    ring: Rgba,
    ring_hover: Rgba,
    index: usize,
) -> AnyElement {
    icon_with_status_sized(icon, status, ring, ring_hover, index, theme::SIDEBAR_ICON_SIZE)
}

/// [`icon_with_status`] for an icon `size` pixels wide: the dot and its ring
/// scale with it (the connection page's header).
pub(super) fn icon_with_status_sized(
    icon: AnyElement,
    status: &ConnectionStatus,
    ring: Rgba,
    ring_hover: Rgba,
    index: usize,
    size: f32,
) -> AnyElement {
    let scale = size / theme::SIDEBAR_ICON_SIZE;
    let (dot_size, ring_width) = (DOT_SIZE * scale, RING_WIDTH * scale);
    let (color, connecting) = match status {
        ConnectionStatus::Connected => (theme::success(), false),
        ConnectionStatus::Connecting => (theme::warning(), true),
        ConnectionStatus::Disconnected | ConnectionStatus::Failed(_) => return icon,
    };
    let dot = div().size(px(dot_size)).rounded_full().bg(color);
    let dot = if connecting {
        dot.with_animation(
            ("nav-connecting", index),
            Animation::new(Duration::from_millis(1400))
                .repeat_synced()
                .with_max_fps(PULSE_FPS)
                // Full strength at both ends, so reduced motion (which draws the last
                // frame) shows the dot at full strength.
                .with_easing(|t| 0.675 + 0.325 * (t * std::f32::consts::TAU).cos()),
            |dot, alpha| dot.opacity(alpha),
        )
        .into_any_element()
    } else {
        dot.into_any_element()
    };
    let overhang = ring_width * 2.0;
    div()
        .relative()
        .flex_shrink_0()
        .size(px(size))
        .child(icon)
        .child(
            div()
                .debug_selector(move || {
                    format!(
                        "sidebar-status-dot-{}-{index}",
                        if connecting { "connecting" } else { "connected" }
                    )
                })
                .absolute()
                .right(px(-overhang))
                .bottom(px(-overhang))
                .size(px(dot_size + ring_width * 2.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .bg(ring)
                .group_hover(ROW_GROUP, move |style| style.bg(ring_hover))
                .child(dot),
        )
        .into_any_element()
}

impl AppRoot {
    /// The failure marker of row `index`: a triangle with a tooltip, and the
    /// popover it opens.
    pub(super) fn render_row_problem(
        &self,
        index: usize,
        row: &NavigatorRow,
        problem: RowProblem,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self.navigator_ui.problem_open.as_ref() == Some(&row.id);
        let connection = matches!(row.id, NavigatorNodeId::Connection { .. });
        let root = cx.entity().downgrade();
        let trigger = Button::new(("nav-problem", index))
            .debug_selector(move || format!("sidebar-problem-{index}"))
            .icon(
                Icon::new(IconName::TriangleAlert)
                    .size(px(13.0))
                    .text_color(theme::danger()),
            )
            .ghost()
            .xsmall()
            .compact();
        let summary = problem_summary(&problem.message);
        let target = Rc::new((row.clone(), problem.message));
        let id = row.id.clone();
        let close_root = root.clone();
        let popover = Popover::new(("nav-problem-popover", index))
            .trigger(trigger)
            .open(open)
            .p(px(0.0))
            .bg(theme::surface())
            .on_open_change(move |open, _, app| {
                let open = *open;
                let id = id.clone();
                let _ = close_root.update(app, |this, cx| {
                    let current = &mut this.navigator_ui.problem_open;
                    if open {
                        *current = Some(id);
                    } else if current.as_ref() == Some(&id) {
                        *current = None;
                    }
                    cx.notify();
                });
            })
            .content(move |_, _, _| problem_panel(index, connection, &target, &root));
        // The tooltip sits on a wrapper: the button's own one is text only,
        // and the tests read this one through its selector.
        div()
            .id(("nav-problem-tip", index))
            .flex_shrink_0()
            .mr(px(6.0))
            .tooltip(move |window, cx| {
                let summary = summary.clone();
                Tooltip::element(move |_, _| {
                    let selector = format!("sidebar-problem-tooltip {summary}");
                    div()
                        .debug_selector(move || selector.clone())
                        .max_w(px(360.0))
                        .child(summary.clone())
                })
                .build(window, cx)
            })
            .child(popover)
            .into_any_element()
    }

    /// Retry, Copy or Clear of a row's failure marker.
    pub(super) fn run_row_problem_action(
        &mut self,
        row: &NavigatorRow,
        message: &str,
        action: ProblemAction,
        cx: &mut Context<Self>,
    ) {
        match action {
            ProblemAction::Copy => {
                cx.write_to_clipboard(ClipboardItem::new_string(message.to_string()));
                return;
            }
            ProblemAction::Retry => self.retry_row_problem(row, cx),
            ProblemAction::Clear => self.clear_row_problem(row, cx),
        }
        self.navigator_ui.problem_open = None;
        self.navigator_ui.generation = self.navigator.generation();
        self.notify_chrome(cx);
    }

    /// The reload that clicking the row would start: a node whose load failed
    /// reloads its children (connecting first when needed); a connection that
    /// was lost, or failed before it had any children to load, reconnects.
    fn retry_row_problem(&mut self, row: &NavigatorRow, cx: &mut Context<Self>) {
        let load_failed = matches!(row.load, Some(NodeLoadState::Failed(_)));
        match &row.id {
            NavigatorNodeId::Connection { connection } if !(load_failed && row.expandable) => {
                self.ensure_connected(connection.clone(), None, cx);
            }
            _ => self.handle_navigator_disclosure(row, cx),
        }
    }

    /// Forgets the failure: a connection goes back to Disconnected, a node
    /// back to unloaded and collapsed.
    fn clear_row_problem(&mut self, row: &NavigatorRow, cx: &mut Context<Self>) {
        if let NavigatorNodeId::Connection { connection } = &row.id {
            if matches!(self.sessions.status(connection), ConnectionStatus::Failed(_)) {
                // Only the shared session failed: the tabs' own sessions keep their transactions.
                self.release_dead_session(connection, cx);
                if self.scenario_ui.service.is_some() {
                    // The scenario's release only drops the tunnel.
                    self.sessions.disconnect(connection);
                    self.navigator_ui.pending_loads.remove(connection);
                    self.sync_sidebar_connections(cx);
                }
            }
        }
        self.navigator.reset_node(&row.id);
    }
}

/// The popover body: the whole error and its actions.
fn problem_panel(
    index: usize,
    connection: bool,
    target: &Rc<(NavigatorRow, SharedString)>,
    root: &WeakEntity<AppRoot>,
) -> AnyElement {
    let (row, message) = (&target.0, &target.1);
    let action = |label: &'static str, id: &'static str, kind: ProblemAction| {
        let (target, root) = (target.clone(), root.clone());
        Button::new((id, index))
            .debug_selector(move || format!("{id}-{index}"))
            .label(label)
            .compact()
            .on_click(move |_, _, app| {
                let _ = root.update(app, |this, cx| {
                    this.run_row_problem_action(&target.0, &target.1, kind, cx);
                });
            })
    };
    div()
        .debug_selector(move || format!("sidebar-problem-popover-{index}"))
        .w(px(320.0))
        .p(px(12.0))
        .flex()
        .flex_col()
        .gap(px(8.0))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .text_size(px(12.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(theme::text())
                .child(
                    Icon::new(IconName::TriangleAlert)
                        .size(px(13.0))
                        .text_color(theme::danger()),
                )
                .child(if connection {
                    "Connection failed"
                } else {
                    "Could not load"
                })
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .font_weight(gpui::FontWeight::NORMAL)
                        .text_color(theme::text_muted())
                        .child(row.label.clone()),
                ),
        )
        .child(
            div()
                .id(("nav-problem-scroll", index))
                .debug_selector(move || format!("sidebar-problem-text-{index}"))
                .max_h(px(140.0))
                .overflow_y_scroll()
                .text_size(px(12.0))
                .child(
                    TextView::markdown(
                        ("nav-problem-text", index),
                        problem_markdown(message),
                    )
                    .selectable(true),
                ),
        )
        .child(
            div()
                .flex()
                .justify_end()
                .gap(px(6.0))
                .child(action("Clear", "sidebar-problem-clear", ProblemAction::Clear))
                .child(action("Copy", "sidebar-problem-copy", ProblemAction::Copy))
                .child(
                    action("Retry", "sidebar-problem-retry", ProblemAction::Retry).primary(),
                ),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_summary_is_the_first_line_cut_to_a_tooltip() {
        assert_eq!(problem_summary("refused\nsecond line"), "refused");
        assert_eq!(problem_summary("\n  \n  spaced  \nnext"), "spaced");
        assert_eq!(problem_summary(""), "");
        let long = "x".repeat(300);
        let cut = problem_summary(&long);
        assert_eq!(cut.chars().count(), SUMMARY_MAX_CHARS + 1);
        assert!(cut.ends_with('…'));
        let exact = "é".repeat(SUMMARY_MAX_CHARS);
        assert_eq!(problem_summary(&exact), exact.as_str(), "no ellipsis when it fits");
    }

    #[test]
    fn the_markdown_keeps_every_line_and_escapes_its_punctuation() {
        assert_eq!(problem_markdown("a\n\n  b `c` *d* 1."), "a  \nb \\`c\\` \\*d\\* 1\\.");
    }
}
