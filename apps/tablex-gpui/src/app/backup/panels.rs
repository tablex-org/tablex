//! How the backup and restore wizard looks: the header and the steps, one panel per step and the footer with the
//! buttons. Everything standard is a `gpui-component` control (`Select`, `Input`, `Checkbox`, `Button`, `Progress`); the
//! lists (the tables, the contents of an archive, the log of the tool) are virtualized.

use gpui::{Context, SharedString, Window};
use gpui_component::progress::Progress;
use gpui_component::select::Select;

use super::super::data_import::view::{grouped, human_bytes};
use super::super::*;
use super::model::{ArchiveState, Flag, Loading, Mode, Scope, Step, ToolState};
use super::view::{BackupView, Running};
use crate::backup::command::PgFormat;
use crate::backup::job::{Operation, Outcome, Report};
use crate::backup::process::{LogLine, Stream};
use crate::script_run::format_duration;

const ROW_HEIGHT: f32 = 24.0;
const LOG_ROW_HEIGHT: f32 = 18.0;

/// A labelled field of the option panels.
fn field(label: &'static str, width: f32, control: impl IntoElement) -> Div {
    div().flex().flex_col().gap(px(3.0)).w(px(width)).child(div().text_size(px(11.0)).text_color(theme::text_muted()).child(label)).child(control)
}

fn note(selector: &'static str, text: impl Into<SharedString>, color: gpui::Rgba) -> Div {
    div().debug_selector(move || selector.into()).text_size(px(11.5)).text_color(color).child(text.into())
}

const SCRIPT_REASON: &str = "A SQL script is restored with Run SQL File, not with a command-line client. It reads the file as it runs, however large it is; it can continue from the statement where a failed run stopped; it can run in one transaction or commit every N statements; and it lists every statement that failed with the server's message instead of stopping at the first one or hiding them in a log.";

impl BackupView {
    fn archive_label(&self) -> String {
        self.archive.as_ref().and_then(|archive| archive.path.file_name()).map_or_else(|| "no backup chosen".to_owned(), |name| name.to_string_lossy().into_owned())
    }

    fn place_label(&self) -> String {
        format!("{} \u{00B7} {}", self.connection_name(), self.target.label())
    }

    fn operation(&self) -> &'static str {
        match self.mode {
            Mode::Backup => "Backup",
            Mode::Restore => "Restore",
        }
    }

    fn render_header(&self) -> impl IntoElement {
        let (icon, title) = match self.mode {
            Mode::Backup => ("icons/download.svg", format!("Backup of {}", self.database())),
            Mode::Restore => ("icons/upload.svg", format!("Restore into {}", self.database())),
        };
        let detail = match self.mode {
            Mode::Backup => self.place_label(),
            Mode::Restore => format!("{} \u{00B7} {}", self.place_label(), self.archive_label()),
        };
        div()
            .h(px(48.0))
            .flex_shrink_0()
            .px(px(12.0))
            .flex()
            .items_center()
            .gap(px(10.0))
            .bg(theme::chrome())
            .border_b_1()
            .border_color(theme::border())
            .child(Icon::empty().path(icon).size(px(16.0)).text_color(theme::accent()))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(div().debug_selector(|| "backup-title".into()).truncate().text_size(px(13.0)).child(title))
                    .child(div().debug_selector(|| "backup-subtitle".into()).truncate().text_size(px(11.0)).text_color(theme::text_muted()).child(detail)),
            )
    }

    fn render_steps(&self) -> impl IntoElement {
        let step = |number: &'static str, label: &'static str, this: Step| {
            let current = self.step == this;
            div()
                .debug_selector(move || format!("backup-step-{}", label.to_lowercase()))
                .flex()
                .items_center()
                .gap(px(6.0))
                .text_color(if current { theme::accent() } else { theme::text_faint() })
                .child(div().w(px(18.0)).h(px(18.0)).rounded(px(9.0)).border_1().border_color(if current { theme::accent() } else { theme::border() }).flex().items_center().justify_center().text_size(px(10.0)).child(number))
                .child(div().text_size(px(12.0)).child(label))
        };
        div()
            .h(px(32.0))
            .flex_shrink_0()
            .px(px(12.0))
            .flex()
            .items_center()
            .gap(px(18.0))
            .border_b_1()
            .border_color(theme::divider())
            .child(step("1", "Source", Step::Source))
            .child(step("2", "Settings", Step::Settings))
            .child(step("3", "Run", Step::Run))
    }

    // ---- 1. Source -----------------------------------------------------------------------------------------

    fn render_source(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let place_label = match self.mode {
            Mode::Backup => "Database or schema to back up",
            Mode::Restore => "Restore into database",
        };
        let panel = div()
            .debug_selector(|| "backup-source-panel".into())
            .flex_1()
            .min_h_0()
            .px(px(12.0))
            .py(px(10.0))
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(10.0))
                    .child(field("Connection", 280.0, div().debug_selector(|| "backup-connection".into()).child(Select::new(&self.connection_select).small().w_full().menu_max_h(px(260.0)))))
                    .child(field(place_label, 280.0, div().debug_selector(|| "backup-namespace".into()).child(Select::new(&self.namespace_select).small().w_full().menu_max_h(px(260.0)))))
                    .when(self.mode == Mode::Backup, |this| this.child(field("What to back up", 220.0, div().debug_selector(|| "backup-scope".into()).child(Select::new(&self.scope_select).small().w_full())))),
            );
        let panel = panel.when(self.database().is_empty(), |this| {
            this.child(note("backup-no-database", "No database is chosen: expand the connection in the navigator so its databases are listed, then choose one above.", theme::warning()))
        });
        match self.mode {
            Mode::Backup => panel.child(self.render_scope_list(cx)),
            Mode::Restore => panel.child(self.render_archive(cx)),
        }
    }

    fn render_scope_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let scope = self.scope(cx);
        if scope == Scope::Whole {
            return note("backup-scope-note", "Every table, view and routine of the database is included.", theme::text_muted()).into_any_element();
        }
        let hint = match scope {
            Scope::Schemas => "Only the schemas the navigator has loaded are listed: expand the database in the navigator to see them all.",
            _ => "The tables of the schema chosen above, as the navigator lists them.",
        };
        let body = match &self.listing {
            Loading::Idle | Loading::Pending => div().p(px(4.0)).text_color(theme::text_muted()).child("Reading the list\u{2026}").into_any_element(),
            Loading::Failed(message) => div().debug_selector(|| "backup-list-error".into()).p(px(4.0)).text_color(theme::danger()).child(message.clone()).into_any_element(),
            Loading::Ready if self.list.items.is_empty() => div().debug_selector(|| "backup-list-empty".into()).p(px(4.0)).text_color(theme::text_muted()).child("Nothing to choose from here.").into_any_element(),
            Loading::Ready => self.render_checklist(cx).into_any_element(),
        };
        div().flex_1().min_h_0().flex().flex_col().gap(px(6.0)).child(note("backup-list-hint", hint, theme::text_muted())).child(div().flex_1().min_h_0().flex().flex_col().child(body)).into_any_element()
    }

    /// The filter, the buttons and the virtualized rows of the checklist.
    fn render_checklist(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tab_id = self.tab_id;
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(div().debug_selector(|| "backup-filter".into()).w(px(260.0)).child(Input::new(&self.filter).small()))
                    .child(Button::new(("backup-all", tab_id)).debug_selector(|| "backup-select-all".into()).label("Select all").compact().ghost().on_click(cx.listener(|this, _, _, cx| this.select_shown(true, cx))))
                    .child(Button::new(("backup-none", tab_id)).debug_selector(|| "backup-select-none".into()).label("Clear").compact().ghost().on_click(cx.listener(|this, _, _, cx| this.select_shown(false, cx))))
                    .child(div().debug_selector(|| "backup-count".into()).text_size(px(11.5)).text_color(theme::text_muted()).child(format!("{} of {} selected", grouped(self.list.selected() as u64), grouped(self.list.items.len() as u64)))),
            )
            .child(
                div().debug_selector(|| "backup-list".into()).flex_1().min_h_0().border_1().border_color(theme::border()).rounded(px(4.0)).child(
                    uniform_list(
                        "backup-list-rows",
                        self.list.shown.len(),
                        cx.processor(|this, range: Range<usize>, _window, cx| {
                            #[cfg(test)]
                            this.drawn.set([this.drawn.get()[0], (range.start, range.end)]);
                            range.filter_map(|position| this.list_row(position, cx)).collect::<Vec<_>>()
                        }),
                    )
                    .size_full()
                    .track_scroll(&self.list_scroll),
                ),
            )
    }

    fn list_row(&self, position: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        let index = *self.list.shown.get(position)?;
        let item = self.list.items.get(index)?;
        let tab_id = self.tab_id;
        Some(
            div()
                .id(("backup-list-row", position))
                .h(px(ROW_HEIGHT))
                .px(px(8.0))
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(div().debug_selector(move || format!("backup-item-{position}")).child(
                    Checkbox::new(gpui::ElementId::Name(format!("backup-pick-{tab_id}-{index}").into())).checked(item.selected).on_click(cx.listener(move |this, checked: &bool, _, cx| this.set_checked(index, *checked, cx))),
                ))
                .child(div().flex_1().min_w_0().truncate().text_size(px(12.0)).child(item.label.clone()))
                .into_any_element(),
        )
    }

    fn render_archive(&self, cx: &mut Context<Self>) -> AnyElement {
        let tab_id = self.tab_id;
        let status = match &self.archive {
            None => Some(("Choose the backup to restore: a custom, tar or directory archive of pg_dump, or an SQL script.".to_owned(), theme::text_muted())),
            Some(archive) => match &archive.state {
                ArchiveState::Detecting => Some(("Looking at the file\u{2026}".to_owned(), theme::text_muted())),
                ArchiveState::Failed(message) => Some((message.clone(), theme::danger())),
                ArchiveState::Ready(kind) => Some((format!("{} \u{00B7} {}", archive.path.display(), kind.label()), theme::text())),
            },
        };
        let mut panel = div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(Button::new(("backup-file", tab_id)).debug_selector(|| "backup-choose-file".into()).label("Choose backup file\u{2026}").compact().on_click(cx.listener(|this, _, window, cx| this.browse_archive(false, window, cx))))
                    .when(self.postgres(), |this| {
                        this.child(Button::new(("backup-folder", tab_id)).debug_selector(|| "backup-choose-folder".into()).label("Choose directory backup\u{2026}").compact().ghost().on_click(cx.listener(|this, _, window, cx| this.browse_archive(true, window, cx))))
                    }),
            )
            .children(status.map(|(text, color)| note("backup-archive", text, color)));
        let script = self.script().is_some();
        if script || !self.postgres() {
            panel = panel.child(
                div()
                    .debug_selector(|| "backup-script-note".into())
                    .p(px(10.0))
                    .rounded(px(4.0))
                    .border_1()
                    .border_color(theme::border())
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .child(div().text_size(px(12.0)).child(if self.postgres() { "This is an SQL script." } else { "MySQL and MariaDB backups are SQL scripts." }))
                    .child(div().text_size(px(11.5)).text_color(theme::text_muted()).child(SCRIPT_REASON))
                    .when(script, |this| {
                        this.child(div().flex().child(Button::new(("backup-script", tab_id)).debug_selector(|| "backup-open-script".into()).label("Open in Run SQL File").primary().compact().on_click(cx.listener(|this, _, window, cx| this.open_script(window, cx)))))
                    }),
            );
        }
        if self.archive_for_other_server() {
            panel = panel.child(note("backup-wrong-server", "This is a pg_dump archive: it restores into PostgreSQL only. Choose a PostgreSQL connection, or an SQL script for this one.", theme::danger()));
        }
        if self.postgres() && self.restorable().is_some() {
            panel = panel.child(note("backup-archive-note", "pg_restore restores this archive. Next lists what is in it.", theme::text_muted()));
        }
        panel.into_any_element()
    }

    // ---- 2. Settings ---------------------------------------------------------------------------------------

    fn render_flag(&self, flag: Flag, cx: &mut Context<Self>) -> impl IntoElement {
        let tab_id = self.tab_id;
        let selector = flag.selector();
        let enabled = self.flag_enabled(flag, cx);
        let on = self.flag(flag) && enabled;
        div()
            .debug_selector(move || selector.into())
            .child(Checkbox::new((selector, tab_id)).label(flag.label()).checked(on).disabled(!enabled).on_click(cx.listener(move |this, checked: &bool, _, cx| this.toggle(flag, *checked, cx))))
    }

    fn render_settings(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tab_id = self.tab_id;
        let backup = self.mode == Mode::Backup;
        let flags: Vec<AnyElement> = self.visible_flags().iter().map(|flag| self.render_flag(*flag, cx).into_any_element()).collect();
        let format = self.format(cx);
        let mut panel = div().debug_selector(|| "backup-settings-panel".into()).flex_1().min_h_0().p(px(12.0)).flex().flex_col().gap(px(12.0));
        if backup && self.postgres() {
            panel = panel.child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(12.0))
                    .child(field("Format", 300.0, div().debug_selector(|| "backup-format".into()).child(Select::new(&self.format_select).small().w_full())))
                    .child(field("Content", 220.0, div().debug_selector(|| "backup-content".into()).child(Select::new(&self.content_select).small().w_full())))
                    .when(self.shows_compression(cx), |this| this.child(field("Compression (0 to 9)", 150.0, div().debug_selector(|| "backup-compression".into()).child(Input::new(&self.compression).small()))))
                    .when(self.shows_jobs(cx), |this| this.child(field("Parallel jobs", 120.0, div().debug_selector(|| "backup-jobs".into()).child(Input::new(&self.jobs).small())))),
            );
        } else if !backup && self.shows_jobs(cx) {
            panel = panel.child(div().flex().child(field("Parallel jobs", 120.0, div().debug_selector(|| "backup-jobs".into()).child(Input::new(&self.jobs).small()))));
        }
        panel = panel.child(div().debug_selector(|| "backup-flags".into()).flex().flex_col().gap(px(6.0)).children(flags));
        if backup && self.postgres() && format != PgFormat::Plain {
            panel = panel.child(note("backup-archive-flags-note", "Dropping objects first and CREATE DATABASE belong to the restore of an archive: choose them there.", theme::text_muted()));
        }
        if backup {
            let output = self.output.as_ref().map(|path| path.display().to_string());
            let output_label = if self.postgres() && format == PgFormat::Directory { "Output folder" } else { "Output file" };
            panel = panel
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(4.0))
                        .child(div().text_size(px(11.0)).text_color(theme::text_muted()).child(output_label))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(8.0))
                                .child(Button::new(("backup-output", tab_id)).debug_selector(|| "backup-choose-output".into()).label("Choose\u{2026}").compact().on_click(cx.listener(|this, _, window, cx| this.choose_output(window, cx))))
                                .child(
                                    div()
                                        .debug_selector(|| "backup-output".into())
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_size(px(12.0))
                                        .text_color(if output.is_some() { theme::text() } else { theme::text_muted() })
                                        .child(output.unwrap_or_else(|| self.default_file_name(cx))),
                                ),
                        ),
                )
                .child(note("backup-output-note", "The backup is written next to the chosen name and moved into place when it is complete, so a failed or cancelled run never leaves a half file there, and never replaces an older backup.", theme::text_muted()));
        } else {
            panel = panel.child(self.render_toc(cx));
        }
        panel
    }

    fn render_toc(&self, cx: &mut Context<Self>) -> AnyElement {
        match &self.toc_loading {
            Loading::Idle | Loading::Pending => note("backup-toc", "Reading what is in the backup\u{2026}", theme::text_muted()).into_any_element(),
            Loading::Failed(message) => note("backup-toc-error", message.clone(), theme::danger()).into_any_element(),
            Loading::Ready => div()
                .debug_selector(|| "backup-toc".into())
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .gap(px(6.0))
                .child(note("backup-toc-hint", "What to restore. Every object is selected; clear the ones to leave out. Leaving out a table's definition but not its data makes the restore fail.", theme::text_muted()))
                .child(self.render_checklist(cx))
                .into_any_element(),
        }
    }

    // ---- 3. Run --------------------------------------------------------------------------------------------

    fn render_run(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let body = if let Some(run) = &self.run {
            self.render_progress(run, cx).into_any_element()
        } else if let Some(report) = &self.report {
            self.render_summary(report, cx).into_any_element()
        } else {
            self.render_ready().into_any_element()
        };
        div().debug_selector(|| "backup-run-panel".into()).flex_1().min_h_0().flex().flex_col().child(body)
    }

    fn render_ready(&self) -> impl IntoElement {
        let (tool, color) = match &self.tool {
            ToolState::Unknown | ToolState::Checking => ("Looking for the tool\u{2026}".to_owned(), theme::text_muted()),
            ToolState::Found(found) => (format!("Runs {}", found.describe()), theme::text_muted()),
            ToolState::Failed(message) => (message.clone(), theme::danger()),
        };
        let line = match self.mode {
            Mode::Backup => format!("A backup of {} on {} will be written to {}.", self.database(), self.connection_name(), self.output.as_ref().map_or_else(String::new, |path| path.display().to_string())),
            Mode::Restore => format!("{} will be restored into {} on {}.", self.archive_label(), self.database(), self.connection_name()),
        };
        div()
            .p(px(12.0))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(div().debug_selector(|| "backup-ready".into()).text_size(px(12.5)).child(line))
            .child(note("backup-tool", tool, color))
            .child(note("backup-secret-note", "The password is given to the tool through a private file that is deleted afterwards, never on the command line.", theme::text_muted()))
            .when(self.mode == Mode::Restore, |this| {
                this.child(note(
                    "backup-restore-note",
                    "Objects the backup holds are created in the database, and with the drop option they replace the ones that exist. A restore is not undone by cancelling it.",
                    theme::text_muted(),
                ))
            })
    }

    fn render_progress(&self, run: &Running, cx: &mut Context<Self>) -> impl IntoElement {
        let snapshot = &run.last;
        let seconds = run.started.elapsed().as_secs_f64();
        let headline = match (run.cancelling, self.mode) {
            (true, _) => "Cancelling\u{2026}",
            (false, Mode::Backup) => "Backing up\u{2026}",
            (false, Mode::Restore) => "Restoring\u{2026}",
        };
        let elapsed = format_duration(std::time::Duration::from_secs_f64(seconds));
        let counters = match self.mode {
            Mode::Backup => format!("{} written \u{00B7} {}/s \u{00B7} {elapsed}", human_bytes(snapshot.bytes), human_bytes(if seconds > 0.0 { (snapshot.bytes as f64 / seconds) as u64 } else { 0 })),
            Mode::Restore => format!("{} lines from the tool \u{00B7} {elapsed}", grouped(snapshot.lines_seen)),
        };
        div()
            .flex_1()
            .min_h_0()
            .p(px(12.0))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(div().text_size(px(13.0)).child(headline))
            .child(div().debug_selector(|| "backup-progress-bar".into()).w_full().child(Progress::new(("backup-bar", self.tab_id)).loading(true)))
            .child(
                div()
                    .debug_selector(|| "backup-progress".into())
                    .flex()
                    .flex_col()
                    .gap(px(3.0))
                    .text_size(px(12.0))
                    .text_color(theme::text_muted())
                    .child(counters)
                    .children(snapshot.last.clone().map(|last| div().truncate().child(last))),
            )
            .child(self.render_log(cx))
    }

    fn render_summary(&self, report: &Report, cx: &mut Context<Self>) -> impl IntoElement {
        let (headline, color) = match (&report.outcome, report.operation) {
            (Outcome::Completed, Operation::Backup) => (format!("Backup complete \u{00B7} {}", human_bytes(report.bytes)), theme::success()),
            (Outcome::Completed, Operation::Restore) => ("Restore finished".to_owned(), theme::success()),
            (Outcome::Cancelled, _) => ("Cancelled".to_owned(), theme::warning()),
            (Outcome::Failed(message), _) => (format!("Failed: {message}"), theme::danger()),
        };
        let kept = match (&report.outcome, report.operation) {
            (Outcome::Completed, Operation::Backup) => report.output.as_ref().map(|path| format!("Written to {}", path.display())),
            (Outcome::Completed, Operation::Restore) => Some("Warnings the tool printed are in the log. The navigator is refreshed.".to_owned()),
            (_, Operation::Backup) => Some("The unfinished file was removed; an older backup of the same name is untouched.".to_owned()),
            (_, Operation::Restore) => Some("What the tool had restored stays: check the database before restoring again.".to_owned()),
        };
        div()
            .debug_selector(|| "backup-summary".into())
            .flex_1()
            .min_h_0()
            .p(px(12.0))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(div().debug_selector(|| "backup-headline".into()).text_size(px(13.0)).text_color(color).child(headline))
            .children(kept.map(|text| div().debug_selector(|| "backup-kept".into()).text_size(px(12.0)).child(text)))
            .when(matches!(report.outcome, Outcome::Failed(_)) && !report.errors.is_empty(), |this| {
                this.child(div().debug_selector(|| "backup-errors".into()).flex().flex_col().text_size(px(11.5)).text_color(theme::danger()).children(report.errors.iter().map(|line| div().truncate().child(line.clone()))))
            })
            .children(report.leftover.clone().map(|text| div().debug_selector(|| "backup-leftover".into()).text_size(px(12.0)).text_color(theme::warning()).child(text)))
            .child(div().debug_selector(|| "backup-stats".into()).text_size(px(12.0)).text_color(theme::text_muted()).child(Self::stats_line(report)))
            .children(report.command.clone().map(|command| div().debug_selector(|| "backup-command".into()).truncate().text_size(px(11.5)).text_color(theme::text_muted()).child(command)))
            .child(self.render_log(cx))
    }

    fn stats_line(report: &Report) -> String {
        let mut parts = Vec::new();
        parts.extend(report.tool.clone());
        parts.extend(report.via.as_ref().map(|via| format!("through {via}")));
        parts.push(format_duration(report.elapsed));
        parts.join(" \u{00B7} ")
    }

    /// The tool's output: the last lines it printed, virtualized.
    fn render_log(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.log.as_ref().map_or(0, |log| log.log_len());
        let dropped = self.log.as_ref().map_or(0, |log| log.snapshot().dropped);
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(div().debug_selector(|| "backup-log-title".into()).text_size(px(11.5)).text_color(theme::text_muted()).child(if dropped > 0 {
                format!("Output of the tool (the last {} lines; {} earlier ones are not kept)", grouped(rows as u64), grouped(dropped))
            } else {
                "Output of the tool".to_owned()
            }))
            .child(
                div().debug_selector(|| "backup-log".into()).flex_1().min_h_0().border_1().border_color(theme::border()).rounded(px(4.0)).child(
                    uniform_list(
                        "backup-log-rows",
                        rows,
                        cx.processor(move |this, range: Range<usize>, _window, _cx| {
                            #[cfg(test)]
                            this.drawn.set([(range.start, range.end), this.drawn.get()[1]]);
                            let Some(log) = &this.log else { return Vec::new() };
                            let start = range.start;
                            log.range(range).into_iter().enumerate().map(|(offset, line)| log_row(start + offset, &line)).collect::<Vec<_>>()
                        }),
                    )
                    .size_full()
                    .track_scroll(&self.log_scroll),
                ),
            )
    }

    // ---- the buttons ---------------------------------------------------------------------------------------

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tab_id = self.tab_id;
        let button = |id: &'static str, label: String| Button::new((id, tab_id)).debug_selector(move || id.into()).label(label).compact();
        let mut buttons: Vec<AnyElement> = Vec::new();
        if let Some(run) = &self.run {
            buttons.push(button("backup-cancel", "Cancel".to_owned()).danger().disabled(run.cancelling).on_click(cx.listener(|this, _, _, cx| this.cancel(cx))).into_any_element());
        } else if self.report.is_some() {
            let again = if self.mode == Mode::Backup { "Back up again" } else { "Restore another" };
            buttons.push(button("backup-again", again.to_owned()).ghost().on_click(cx.listener(|this, _, window, cx| this.again(window, cx))).into_any_element());
            buttons.push(button("backup-close", "Close".to_owned()).on_click(cx.listener(|this, _, window, cx| this.close_tab(window, cx))).into_any_element());
        } else {
            if self.step != Step::Source {
                buttons.push(button("backup-back", "Back".to_owned()).ghost().on_click(cx.listener(|this, _, _, cx| this.back(cx))).into_any_element());
            }
            match self.step {
                Step::Source => buttons.push(button("backup-next", "Next".to_owned()).primary().disabled(!self.can_leave_source()).on_click(cx.listener(|this, _, window, cx| this.next(window, cx))).into_any_element()),
                Step::Settings => buttons.push(button("backup-next", "Next".to_owned()).primary().on_click(cx.listener(|this, _, window, cx| this.next(window, cx))).into_any_element()),
                Step::Run => {
                    let ready = matches!(self.tool, ToolState::Found(_)) && self.work(cx).is_ok();
                    buttons.push(button("backup-start", self.operation().to_owned()).primary().disabled(!ready).on_click(cx.listener(|this, _, window, cx| this.begin_run(window, cx))).into_any_element());
                }
            }
        }
        div()
            .h(px(44.0))
            .flex_shrink_0()
            .px(px(12.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .border_t_1()
            .border_color(theme::border())
            .bg(theme::chrome())
            .child(div().debug_selector(|| "backup-message".into()).flex_1().min_w_0().truncate().text_size(px(11.5)).text_color(theme::danger()).children(self.message.clone()))
            .children(buttons)
    }
}

/// One line of the log.
fn log_row(index: usize, line: &LogLine) -> AnyElement {
    let color = if line.stream == Stream::Command { theme::accent() } else { theme::text() };
    div()
        .id(("backup-log-row", index))
        .debug_selector(move || format!("backup-log-{index}"))
        .h(px(LOG_ROW_HEIGHT))
        .px(px(8.0))
        .flex()
        .items_center()
        .truncate()
        .text_size(px(11.5))
        .text_color(color)
        .child(line.text.clone())
        .into_any_element()
}

impl Render for BackupView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.poll_listing(cx);
        let tab_id = self.tab_id;
        let panel = match self.step {
            Step::Source => self.render_source(cx).into_any_element(),
            Step::Settings => self.render_settings(cx).into_any_element(),
            Step::Run => self.render_run(cx).into_any_element(),
        };
        div()
            .id(("backup-view", tab_id))
            .debug_selector(|| "backup-view".into())
            .size_full()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(theme::background())
            .text_size(px(12.0))
            .text_color(theme::text())
            .child(self.render_header())
            .child(self.render_steps())
            .child(panel)
            .child(self.render_footer(cx))
    }
}
