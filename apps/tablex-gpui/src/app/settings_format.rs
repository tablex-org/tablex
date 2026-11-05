//! The extra rows of Settings › Editor › Format SQL: whether SQL Formatter (sqruff) is installed
//! (with the way to the Driver Manager when it is not) and the external-command presets whose
//! program is on the PATH. The formatter, the command, its arguments and the timeout are ordinary
//! registry rows.
//!
//! What is installed and what is on the PATH is found once per visit of the page, on a background
//! thread (the PATH of a macOS app comes from the user's login shell: seconds, not
//! milliseconds), never while painting.

use super::*;
use crate::settings::registry::Value;
use gpui::SharedString;
use crate::sql_format::external::{detect_presets, Preset};
use engine_driver_host::{ToolStore, SQRUFF_TOOL_ID};

/// Whether the formatter tool can be chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum FormatterToolState {
    Installed { version: String },
    NotInstalled,
    /// The tools folder or the record is unusable (shown as it is).
    Unusable(String),
}

/// What the page found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FormatScan {
    pub(super) tool: FormatterToolState,
    pub(super) presets: Vec<Preset>,
}

pub(super) fn scan_formatters(store: Option<&ToolStore>) -> FormatScan {
    let tool = match store.map(|store| store.installed(SQRUFF_TOOL_ID)) {
        Some(Ok(Some(record))) => FormatterToolState::Installed { version: record.version },
        Some(Err(error)) => FormatterToolState::Unusable(error.to_string()),
        Some(Ok(None)) | None => FormatterToolState::NotInstalled,
    };
    FormatScan { tool, presets: detect_presets() }
}

impl AppRoot {
    /// Looks again at what is installed and on the PATH; the page keeps showing the last answer
    /// until the new one arrives.
    pub(super) fn scan_format_tools(&mut self, cx: &mut Context<Self>) {
        let store = self.driver_ui.tools.store.clone();
        let job = cx
            .background_executor()
            .spawn(async move { smol::unblock(move || scan_formatters(store.as_deref())).await });
        self.format_ui.scan_task = Some(cx.spawn(async move |this, cx| {
            let scan = job.await;
            let _ = this.update(cx, |this, cx| {
                this.format_ui.scan = Some(scan);
                this.format_ui.scan_task = None;
                cx.notify();
            });
        }));
    }

    /// Looks again when the page has looked before (a tool was installed or removed meanwhile).
    pub(super) fn rescan_format_tools_if_seen(&mut self, cx: &mut Context<Self>) {
        if self.format_ui.scan.is_some() {
            self.scan_format_tools(cx);
        }
    }

    /// Whether choosing SQL Formatter (sqruff) is refused now: the last scan saw that it is not there,
    /// and the Driver Manager does not say it has been installed since.
    pub(super) fn formatter_tool_missing(&self) -> bool {
        self.format_ui
            .scan
            .as_ref()
            .is_some_and(|scan| !matches!(scan.tool, FormatterToolState::Installed { .. }))
            && !self.driver_ui.tools.formatter_installed()
    }

    /// A preset button: the command and its arguments go into the draft and the external command
    /// becomes the formatter.
    pub(super) fn settings_apply_format_preset(&mut self, preset: Preset, cx: &mut Context<Self>) {
        for (id, value) in [
            ("editor.format_external_program", Value::Text(preset.program.to_owned())),
            ("editor.format_external_args", Value::Text(preset.args.to_owned())),
            ("editor.format_external_exit_codes", Value::Text(preset.exit_codes.to_owned())),
            ("editor.formatter", Value::Choice("external")),
        ] {
            self.settings_set(id, value, cx);
            if let Some(page) = self.settings_ui.page.as_mut() {
                page.request_resync(id);
            }
        }
    }

    /// The rows under the Format SQL settings.
    pub(super) fn render_format_tools_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let scan = self.format_ui.scan.as_ref();
        let status: AnyElement = match scan.map(|scan| &scan.tool) {
            None => note("format-tool-status", "Looking for SQL Formatter (sqruff) and the formatters on your PATH\u{2026}"),
            Some(FormatterToolState::Installed { version }) => {
                note("format-tool-status", format!("SQL Formatter (sqruff) {version} is installed."))
            }
            Some(FormatterToolState::NotInstalled) => div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(note("format-tool-status", "SQL Formatter (sqruff) is not installed."))
                .child(
                    Button::new("format-tool-open-driver-manager")
                        .debug_selector(|| "format-tool-open-driver-manager".into())
                        .label("Open Driver Manager")
                        .compact()
                        .on_click(cx.listener(|this, _, _, cx| this.open_driver_manager(cx))),
                )
                .into_any_element(),
            Some(FormatterToolState::Unusable(message)) => {
                note("format-tool-status", format!("SQL Formatter (sqruff) cannot be used: {message}"))
            }
        };
        let presets: AnyElement = match scan {
            None => div().into_any_element(),
            Some(scan) if scan.presets.is_empty() => note(
                "format-presets-none",
                "None of sqruff, sqlfluff, sql-formatter or pg_format was found on your PATH.",
            ),
            Some(scan) => div()
                .debug_selector(|| "format-presets".into())
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(6.0))
                .child(div().text_size(px(12.0)).text_color(theme::text_muted()).child("Found on your PATH:"))
                .children(scan.presets.iter().copied().enumerate().map(|(position, preset)| {
                    Button::new(("format-preset", position))
                        .debug_selector(move || format!("format-preset-{}", preset.id))
                        .label(preset.label)
                        .compact()
                        .tooltip(format!("{} {}", preset.program, preset.args))
                        .on_click(cx.listener(move |this, _, _, cx| this.settings_apply_format_preset(preset, cx)))
                }))
                .into_any_element(),
        };
        div()
            .debug_selector(|| "format-tool-row".into())
            .flex()
            .flex_col()
            .gap(px(6.0))
            .px(px(12.0))
            .py(px(10.0))
            .child(status)
            .child(presets)
            .into_any_element()
    }
}

fn note(selector: &'static str, text: impl Into<SharedString>) -> AnyElement {
    div()
        .debug_selector(move || selector.into())
        .text_size(px(12.0))
        .text_color(theme::text_muted())
        .child(text.into())
        .into_any_element()
}
