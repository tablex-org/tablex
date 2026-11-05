//! What a shell run shows in its tab: a status strip above the result (what it returned, "More",
//! the Output toggle, the error with its line and column), the shell text of a scalar or write
//! result in place of the grid, and the Output pane below it.
//!
//! The Output and the text are lists of lines in one `uniform_list` each (only the visible rows
//! are built, however long the script printed); nothing here owns an entity or a listener per line.

use super::super::*;
use crate::mongo_shell::{failure_label, failure_text};
use gpui::ClipboardItem;
use gpui_component::progress::Progress;

const OUTPUT_HEIGHT: f32 = 150.0;
const LINE_HEIGHT: f32 = 18.0;

impl AppRoot {
    /// The strip between the "Results" header and the result: what the last shell run produced,
    /// or the progress of the plugin install the script waits for.
    pub(in crate::app) fn render_shell_strip(
        &mut self,
        tab_id: u64,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.mongo_shell.waits.contains_key(&tab_id) {
            return Some(self.render_shell_waiting(tab_id));
        }
        let view = self.shell_view(tab_id)?;
        let summary = match (&view.error, &view.value_text) {
            (Some(error), _) => failure_label(error.kind).to_string(),
            (None, Some(_)) => "Shell result \u{00B7} text".to_string(),
            (None, None) if view.documents == 1 => "Shell result \u{00B7} 1 document".to_string(),
            (None, None) if view.documents > 0 => {
                format!("Shell result \u{00B7} {} documents", view.documents)
            }
            (None, None) => "Shell finished \u{00B7} no result".to_string(),
        };
        let database = view.database.clone();
        let more = view.more.is_some();
        let output_lines = view.output.len();
        let output_open = view.output_open;
        let error = view.error.as_ref().map(|failure| {
            // The place is part of the sentence, so a reader sees it inline.
            format!("{}: {}", failure_label(failure.kind), failure_text(failure))
        });
        let notes = view.notes.clone();
        let text_result = view.value_text.is_some();
        let colour = if error.is_some() { theme::danger() } else { theme::text_muted() };
        Some(
            div()
                .debug_selector(|| "shell-strip".into())
                .flex_shrink_0()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .px(px(12.0))
                .py(px(6.0))
                .bg(theme::chrome())
                .border_b_1()
                .border_color(theme::border())
                .text_size(px(12.0))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(10.0))
                        .child(
                            div()
                                .debug_selector(|| "shell-summary".into())
                                .text_color(colour)
                                .child(summary),
                        )
                        .child(
                            div()
                                .debug_selector(|| "shell-database".into())
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(theme::text_faint())
                                .child(format!("db {database}")),
                        )
                        .children(text_result.then(|| {
                            Button::new(("shell-copy-text", tab_id))
                                .debug_selector(|| "shell-copy-text".into())
                                .label("Copy")
                                .compact()
                                .ghost()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.copy_shell_text(tab_id, cx)
                                }))
                        }))
                        .children(more.then(|| {
                            Button::new(("shell-more", tab_id))
                                .debug_selector(|| "shell-more".into())
                                .label("More")
                                .compact()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.shell_more(tab_id, cx)
                                }))
                        }))
                        .children((output_lines > 0).then(|| {
                            Button::new(("shell-output-toggle", tab_id))
                                .debug_selector(|| "shell-output-toggle".into())
                                .label(format!("Output ({output_lines})"))
                                .compact()
                                .ghost()
                                .selected(output_open)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.toggle_shell_output(tab_id, cx)
                                }))
                        })),
                )
                .children(error.map(|message| {
                    div()
                        .debug_selector(|| "shell-error".into())
                        .text_color(theme::danger())
                        .whitespace_normal()
                        .child(message)
                }))
                .children(notes.into_iter().map(|note| {
                    div()
                        .debug_selector(|| "shell-note".into())
                        .text_color(theme::warning())
                        .child(note)
                }))
                .into_any_element(),
        )
    }

    /// "Installing the MongoDB Shell plugin… 43%": the script follows the install.
    fn render_shell_waiting(&self, tab_id: u64) -> AnyElement {
        let tools = &self.driver_ui.tools;
        let (text, fraction) = match tools.job_of(engine_driver_host::MONGO_SHELL_TOOL_ID) {
            Some(job) => (
                format!(
                    "Installing the MongoDB Shell plugin\u{2026} {}% \u{00B7} the script runs when it is in",
                    (job.state.fraction() * 100.0).round() as u32
                ),
                job.state.fraction(),
            ),
            None => (
                "Installing the MongoDB driver first\u{2026} the plugin and the script follow".to_string(),
                self.driver_ui
                    .queue
                    .job(engine_driver_host::MONGODB_DRIVER_ID)
                    .map_or(0.0, |job| job.state.fraction()),
            ),
        };
        div()
            .debug_selector(|| "shell-waiting".into())
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .px(px(12.0))
            .py(px(6.0))
            .bg(theme::chrome())
            .border_b_1()
            .border_color(theme::border())
            .text_size(px(12.0))
            .child(div().text_color(theme::text_muted()).child(text))
            .child(Progress::new(("shell-wait-bar", tab_id)).value(fraction * 100.0))
            .into_any_element()
    }

    pub(in crate::app) fn toggle_shell_output(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if let Some(view) = self
            .workspace
            .runtime_mut(tab_id)
            .and_then(|runtime| runtime.shell.as_mut())
        {
            view.output_open = !view.output_open;
        }
        self.notify_tab(tab_id, cx);
    }

    fn copy_shell_text(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if let Some(text) = self.shell_view(tab_id).and_then(|view| view.value_text.clone()) {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn copy_shell_output(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if let Some(view) = self.shell_view(tab_id) {
            cx.write_to_clipboard(ClipboardItem::new_string(view.output_text()));
        }
    }

    /// The scalar or write result is shown as text where the grid would be.
    pub(in crate::app) fn shell_text_active(&self, tab_id: u64) -> bool {
        self.shell_view(tab_id)
            .is_some_and(|view| view.value_text.is_some() && view.error.is_none())
    }

    /// The shell text of a scalar or write result, one line per row of a virtualized list.
    pub(in crate::app) fn render_shell_text(&mut self, tab_id: u64, cx: &mut Context<Self>) -> AnyElement {
        let lines = self
            .shell_view(tab_id)
            .map_or(0, |view| view.value_lines.len());
        let scroll = self.view(tab_id).shell_text_scroll.clone();
        let rows = uniform_list(
            ("shell-text-rows", tab_id),
            lines,
            cx.processor(move |this, range: Range<usize>, _window, _cx| {
                let Some(view) = this.shell_view(tab_id) else {
                    return Vec::new();
                };
                range
                    .filter_map(|index| view.value_lines.get(index).map(shell_line))
                    .collect()
            }),
        )
        .flex_1()
        .min_h_0()
        .track_scroll(&scroll);
        div()
            .debug_selector(|| "shell-text".into())
            .flex()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .relative()
            .px(px(12.0))
            .py(px(6.0))
            .child(rows)
            .child(Scrollbar::vertical(&scroll))
            .into_any_element()
    }

    /// The Output pane: what `print` and `console.log` wrote, with the cap's note last.
    pub(in crate::app) fn render_shell_output(
        &mut self,
        tab_id: u64,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let view = self.shell_view(tab_id)?;
        if !view.output_open || view.output.is_empty() {
            return None;
        }
        let (count, truncated) = (view.output.len(), view.output_truncated);
        let scroll = self.view(tab_id).shell_output_scroll.clone();
        let rows = uniform_list(
            ("shell-output-rows", tab_id),
            count,
            cx.processor(move |this, range: Range<usize>, _window, _cx| {
                let Some(view) = this.shell_view(tab_id) else {
                    return Vec::new();
                };
                range
                    .filter_map(|index| view.output.get(index).map(shell_line))
                    .collect()
            }),
        )
        .flex_1()
        .min_h_0()
        .track_scroll(&scroll);
        Some(
            div()
                .debug_selector(|| "shell-output".into())
                .flex_shrink_0()
                .h(px(OUTPUT_HEIGHT))
                .flex()
                .flex_col()
                .border_t_1()
                .border_color(theme::border())
                .child(
                    div()
                        .flex_shrink_0()
                        .h(px(26.0))
                        .px(px(12.0))
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .bg(theme::chrome())
                        .text_size(px(11.0))
                        .text_color(theme::text_muted())
                        .child(div().flex_1().child(format!(
                            "Output \u{00B7} {count} line{}{}",
                            if count == 1 { "" } else { "s" },
                            if truncated { " \u{00B7} truncated" } else { "" }
                        )))
                        .child(
                            Button::new(("shell-output-copy", tab_id))
                                .debug_selector(|| "shell-output-copy".into())
                                .label("Copy")
                                .compact()
                                .ghost()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.copy_shell_output(tab_id, cx)
                                })),
                        )
                        .child(
                            Button::new(("shell-output-close", tab_id))
                                .debug_selector(|| "shell-output-close".into())
                                .icon(Icon::default().path("icons/close.svg").size(px(12.0)))
                                .compact()
                                .ghost()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.toggle_shell_output(tab_id, cx)
                                })),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_1()
                        .min_h_0()
                        .relative()
                        .px(px(12.0))
                        .child(rows)
                        .child(Scrollbar::vertical(&scroll)),
                )
                .into_any_element(),
        )
    }
}

/// One line of output or text, in the editor's monospace (a very long one is cut for drawing).
fn shell_line(line: &gpui::SharedString) -> AnyElement {
    let text = match crate::mongo_shell::clip_line(line) {
        std::borrow::Cow::Borrowed(_) => line.clone(),
        std::borrow::Cow::Owned(clipped) => clipped.into(),
    };
    div()
        .h(px(LINE_HEIGHT))
        .whitespace_nowrap()
        .overflow_hidden()
        .font_family("Menlo")
        .text_size(px(12.0))
        .text_color(theme::text())
        .child(text)
        .into_any_element()
}
