//! How the SQL file wizard looks: the header and the steps, one panel per step and the footer with the buttons.
//! Everything standard is a `gpui-component` control (`Select`, `Input`, `Button`, `Progress`); the lists are
//! plain lines (the file is never shown in a text widget), the error list is virtualized.

use gpui::{Context, SharedString, Window};
use gpui_component::progress::Progress;
use gpui_component::select::Select;

use super::super::data_import::view::{grouped, human_bytes};
use super::super::*;
use super::run::{Entry, EntryKind, Outcome, Report, Transactions, ERROR_LOG_CAP};
use super::view::{FileRunView, RunningFile, SampleState, Step};
use crate::script_run::format_duration;

const ENTRY_HEIGHT: f32 = 22.0;

/// A labelled field of the option panels.
fn field(label: &'static str, width: f32, control: impl IntoElement) -> Div {
    div().flex().flex_col().gap(px(3.0)).w(px(width)).child(div().text_size(px(11.0)).text_color(theme::text_muted()).child(label)).child(control)
}

impl FileRunView {
    fn file_label(&self) -> String {
        self.path.as_ref().and_then(|path| path.file_name()).map_or_else(|| "no file chosen".to_owned(), |name| name.to_string_lossy().into_owned())
    }

    fn place_label(&self) -> String {
        format!("{} \u{00B7} {}", self.connection_name(), self.target.label())
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tab_id = self.tab_id;
        let detail = match &self.sample {
            SampleState::Ready(sample) => format!("{} \u{00B7} {}", self.file_label(), human_bytes(sample.size)),
            _ => self.file_label(),
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
            .child(Icon::empty().path("icons/play.svg").size(px(16.0)).text_color(theme::accent()))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(div().debug_selector(|| "filerun-title".into()).truncate().text_size(px(13.0)).child("Run SQL file"))
                    .child(div().debug_selector(|| "filerun-file".into()).truncate().text_size(px(11.0)).text_color(theme::text_muted()).child(detail)),
            )
            .child(
                Button::new(("filerun-browse", tab_id))
                    .debug_selector(|| "filerun-browse".into())
                    .label("Choose file\u{2026}")
                    .compact()
                    .ghost()
                    .disabled(self.run.is_some())
                    .on_click(cx.listener(|this, _, window, cx| this.browse(window, cx))),
            )
    }

    fn render_steps(&self) -> impl IntoElement {
        let step = |number: &'static str, label: &'static str, this: Step| {
            let current = self.step == this;
            div()
                .debug_selector(move || format!("filerun-step-{}", label.to_lowercase()))
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

    fn render_source(&self) -> impl IntoElement {
        let status = match &self.sample {
            SampleState::NoFile => Some(("Choose the SQL file to run.".to_owned(), theme::text_muted())),
            SampleState::Reading => Some(("Reading the start of the file\u{2026}".to_owned(), theme::text_muted())),
            SampleState::Failed(message) => Some((message.clone(), theme::danger())),
            SampleState::Ready(_) => None,
        };
        let sample = self.ready_sample();
        div()
            .debug_selector(|| "filerun-source-panel".into())
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
                    .child(field("Connection", 280.0, div().debug_selector(|| "filerun-connection".into()).child(Select::new(&self.connection_select).small().w_full().menu_max_h(px(260.0)))))
                    .child(field("Database or schema", 280.0, div().debug_selector(|| "filerun-namespace".into()).child(Select::new(&self.namespace_select).small().w_full().menu_max_h(px(260.0)))))
                    .child(field("Encoding", 230.0, div().debug_selector(|| "filerun-encoding".into()).child(Select::new(&self.encoding_select).small().w_full())))
                    .child(field("Start at statement", 140.0, div().debug_selector(|| "filerun-start-at".into()).child(Input::new(&self.start_at).small()))),
            )
            .child(div().text_size(px(11.5)).text_color(theme::text_muted()).child(
                "The file is read as it runs, never loaded whole. To run it again after a failure, start at the statement the last run stopped at: the ones before it are read and left out, except those that only set the session up (USE, SET, \\connect), which run again so the start runs where it would have.",
            ))
            .children(status.map(|(text, color)| div().debug_selector(|| "filerun-source-status".into()).text_size(px(11.5)).text_color(color).child(text)))
            .children(sample.map(|sample| {
                div()
                    .debug_selector(|| "filerun-sample".into())
                    .flex()
                    .flex_col()
                    .border_1()
                    .border_color(theme::border())
                    .rounded(px(4.0))
                    .child(ui::section_header("First statements"))
                    .children(sample.first.iter().enumerate().map(|(index, text)| {
                        div()
                            .debug_selector(move || format!("filerun-sample-{index}"))
                            .h(px(ENTRY_HEIGHT))
                            .px(px(8.0))
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .child(div().w(px(26.0)).flex_shrink_0().text_size(px(11.0)).text_color(theme::text_faint()).child(format!("#{}", index + 1)))
                            .child(div().flex_1().min_w_0().truncate().text_size(px(11.5)).child(text.clone()))
                    }))
                    .children(sample.more.then(|| div().h(px(ENTRY_HEIGHT)).px(px(8.0)).flex().items_center().text_size(px(11.5)).text_color(theme::text_faint()).child("\u{2026}")))
                    .children(sample.first.is_empty().then(|| div().h(px(ENTRY_HEIGHT)).px(px(8.0)).flex().items_center().text_size(px(11.5)).text_color(theme::warning()).child("The file holds no statement.")))
            }))
    }

    // ---- 2. Settings ---------------------------------------------------------------------------------------

    fn render_settings(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let row = |select: &gpui::Entity<gpui_component::select::SelectState<Vec<SharedString>>>| select.read(cx).selected_index(cx).map_or(0, |index| index.row);
        let mode = row(&self.transactions_select);
        let transactional = mode != 0 && self.transactions_available();
        let every = transactional && mode == 2;
        let note = if !self.transactions_available() {
            Some(("This database has no transactions: every statement runs on its own.".to_owned(), theme::text_muted()))
        } else if transactional {
            self.implicit_commit_note().map(|text| (text.to_owned(), theme::warning()))
        } else {
            None
        };
        div()
            .debug_selector(|| "filerun-settings-panel".into())
            .flex_1()
            .min_h_0()
            .p(px(12.0))
            .flex()
            .flex_col()
            .gap(px(12.0))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(12.0))
                    .child(field("Transactions", 300.0, div().debug_selector(|| "filerun-transactions".into()).child(Select::new(&self.transactions_select).small().w_full().disabled(!self.transactions_available()))))
                    .when(every, |this| this.child(field("Statements per commit", 170.0, div().debug_selector(|| "filerun-every".into()).child(Input::new(&self.every).small()))))
                    .child(field("When a statement fails", 260.0, div().debug_selector(|| "filerun-on-error".into()).child(Select::new(&self.on_error_select).small().w_full().disabled(transactional)))),
            )
            .child(div().debug_selector(|| "filerun-transaction-note".into()).text_size(px(11.5)).text_color(theme::text_muted()).child(if transactional {
                "In a transaction the first failed statement ends the run and rolls the open transaction back: the server cannot keep the rest of it. Statements committed before it stay. The file's own BEGIN and COMMIT are left out."
            } else {
                "Autocommit keeps every statement that ran when the run stops. The file's own BEGIN and COMMIT work as written."
            }))
            .children(note.map(|(text, color)| div().debug_selector(|| "filerun-ddl-note".into()).text_size(px(11.5)).text_color(color).child(text)))
            .child(div().text_size(px(11.5)).text_color(theme::text_muted()).child(
                "Statements run on a session of their own. Result sets of SELECT statements are discarded. Lines for the psql client (\\set, \\echo) and COPY \u{2026} FROM stdin blocks are left out and listed; \\connect switches database.",
            ))
    }

    // ---- 3. Run --------------------------------------------------------------------------------------------

    fn render_run(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let body = if let Some(run) = &self.run {
            self.render_progress(run).into_any_element()
        } else if let Some(report) = &self.report {
            self.render_summary(report, cx).into_any_element()
        } else {
            self.render_ready(cx).into_any_element()
        };
        div().debug_selector(|| "filerun-run-panel".into()).flex_1().min_h_0().flex().flex_col().child(body)
    }

    fn mode_text(settings: &super::run::Settings) -> String {
        let transactions = match settings.transactions {
            Transactions::Autocommit => "autocommit".to_owned(),
            Transactions::Whole => "one transaction".to_owned(),
            Transactions::Every(n) => format!("a commit every {} statements", grouped(n)),
        };
        let on_error = match settings.on_error {
            super::run::OnError::Stop => "stops at the first error",
            super::run::OnError::Continue => "logs errors and continues",
        };
        format!("{transactions}, {on_error}")
    }

    fn render_ready(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let size = self.ready_sample().map_or(0, |sample| sample.size);
        let line = match self.settings(cx) {
            Ok(settings) => format!(
                "{} ({}) runs on {}, {}{}.",
                self.file_label(),
                human_bytes(size),
                self.place_label(),
                Self::mode_text(&settings),
                if settings.start_at > 1 { format!(", from statement {}", grouped(settings.start_at)) } else { String::new() }
            ),
            Err(error) => error.to_string(),
        };
        div()
            .p(px(12.0))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(div().debug_selector(|| "filerun-ready".into()).text_size(px(12.5)).child(line))
            .child(div().text_size(px(11.5)).text_color(theme::text_muted()).child("Statements that change or delete data are part of the file and run without asking again."))
    }

    fn render_progress(&self, run: &RunningFile) -> impl IntoElement {
        let snapshot = run.last;
        let seconds = run.started.elapsed().as_secs_f64();
        let fraction = if run.total_bytes > 0 { (snapshot.bytes as f64 / run.total_bytes as f64).clamp(0.0, 1.0) } else { 0.0 };
        let statements = if seconds > 0.0 { (snapshot.ok + snapshot.failed) as f64 / seconds } else { 0.0 };
        let bytes = if seconds > 0.0 { snapshot.bytes as f64 / seconds } else { 0.0 };
        let eta = (fraction > 0.01 && seconds > 2.0).then(|| format!(" \u{00B7} about {} left", format_duration(std::time::Duration::from_secs_f64(seconds * (1.0 - fraction) / fraction))));
        div()
            .p(px(12.0))
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(div().text_size(px(13.0)).child(if run.cancelling { "Cancelling\u{2026}" } else { "Running\u{2026}" }))
            .child(div().debug_selector(|| "filerun-progress-bar".into()).w_full().child(Progress::new(("filerun-bar", self.tab_id)).value(fraction as f32 * 100.0)))
            .child(
                div()
                    .debug_selector(|| "filerun-progress".into())
                    .flex()
                    .flex_col()
                    .gap(px(3.0))
                    .text_size(px(12.0))
                    .text_color(theme::text_muted())
                    .child(format!("Statement {} \u{00B7} {} ok \u{00B7} {} failed \u{00B7} {} skipped", grouped(snapshot.number), grouped(snapshot.ok), grouped(snapshot.failed), grouped(snapshot.skipped)))
                    .child(format!(
                        "{} of {} read \u{00B7} {} statements/s \u{00B7} {}/s \u{00B7} {}{}",
                        human_bytes(snapshot.bytes),
                        human_bytes(run.total_bytes),
                        grouped(statements as u64),
                        human_bytes(bytes as u64),
                        format_duration(std::time::Duration::from_secs_f64(seconds)),
                        eta.unwrap_or_default()
                    )),
            )
    }

    fn render_summary(&self, report: &Report, cx: &mut Context<Self>) -> impl IntoElement {
        let (headline, color) = match &report.outcome {
            Outcome::Completed if report.failed == 0 => (format!("Ran {} statements", grouped(report.ok)), theme::success()),
            Outcome::Completed => (format!("Ran {} statements, {} failed", grouped(report.ok), grouped(report.failed)), theme::warning()),
            Outcome::Stopped(reason) => (format!("Stopped: {reason}"), theme::danger()),
            Outcome::Cancelled => ("Cancelled".to_owned(), theme::warning()),
            Outcome::Failed(message) => (format!("Failed: {message}"), theme::danger()),
        };
        let kept = if report.rolled_back > 0 {
            format!("The open transaction was rolled back: {} statements were undone. What was committed before it stays.", grouped(report.rolled_back))
        } else if report.uncertain || matches!(report.outcome, Outcome::Completed) {
            String::new()
        } else if report.ok > 0 {
            format!("The {} statements that ran stay.", grouped(report.ok))
        } else {
            "Nothing was changed.".to_owned()
        };
        let more = report.entries_total.saturating_sub(report.entries.len() as u64);
        let count = report.entries.len();
        div()
            .debug_selector(|| "filerun-summary".into())
            .flex_1()
            .min_h_0()
            .p(px(12.0))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(div().debug_selector(|| "filerun-headline".into()).text_size(px(13.0)).text_color(color).child(headline))
            .children((!kept.is_empty()).then(|| div().debug_selector(|| "filerun-kept".into()).text_size(px(12.0)).child(kept)))
            .child(div().debug_selector(|| "filerun-stats".into()).text_size(px(12.0)).text_color(theme::text_muted()).child(stats_line(report)))
            .children(report.uncertain.then(|| {
                div().debug_selector(|| "filerun-uncertain".into()).text_size(px(12.0)).text_color(theme::warning()).child(
                    "A COMMIT or ROLLBACK was not confirmed: the statements of the last transaction may or may not be kept. Check the database before running again.",
                )
            }))
            .children(report.resume_from.map(|from| {
                div().debug_selector(|| "filerun-resume-hint".into()).text_size(px(12.0)).child(format!("To run it again without repeating what stands, start at statement {}.", grouped(from)))
            }))
            .children((count > 0).then(|| {
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(div().text_size(px(11.5)).text_color(theme::text_muted()).child(format!(
                        "Failed, skipped, run-again and not-repeated statements{}:",
                        if more > 0 { format!(" (the first {ERROR_LOG_CAP} of {})", grouped(report.entries_total)) } else { String::new() }
                    )))
                    .child(
                        div().debug_selector(|| "filerun-entries".into()).flex_1().min_h_0().border_1().border_color(theme::border()).rounded(px(4.0)).child(
                            uniform_list(
                                "filerun-entry-rows",
                                count,
                                cx.processor(move |this, range: Range<usize>, _window, _cx| {
                                    range.filter_map(|index| this.report.as_ref()?.entries.get(index).map(|entry| entry_row(index, entry))).collect::<Vec<_>>()
                                }),
                            )
                            .size_full()
                            .track_scroll(&self.entries_scroll),
                        ),
                    )
            }))
    }

    // ---- the buttons ---------------------------------------------------------------------------------------

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tab_id = self.tab_id;
        let button = |id: &'static str, label: String| Button::new((id, tab_id)).debug_selector(move || id.into()).label(label).compact();
        let mut buttons: Vec<AnyElement> = Vec::new();
        if let Some(run) = &self.run {
            buttons.push(button("filerun-cancel", "Cancel run".to_owned()).danger().disabled(run.cancelling).on_click(cx.listener(|this, _, _, cx| this.cancel(cx))).into_any_element());
        } else if let Some(report) = &self.report {
            if let Some(from) = report.resume_from {
                buttons.push(button("filerun-resume", format!("Run from statement {}", grouped(from))).primary().on_click(cx.listener(move |this, _, window, cx| this.resume(from, window, cx))).into_any_element());
            }
            buttons.push(button("filerun-again", "Run another file".to_owned()).ghost().on_click(cx.listener(|this, _, window, cx| this.again(window, cx))).into_any_element());
            buttons.push(button("filerun-close", "Close".to_owned()).on_click(cx.listener(|this, _, window, cx| this.close_tab(window, cx))).into_any_element());
        } else {
            if self.step != Step::Source {
                buttons.push(button("filerun-back", "Back".to_owned()).ghost().on_click(cx.listener(|this, _, _, cx| this.back(cx))).into_any_element());
            }
            match self.step {
                Step::Source => buttons.push(button("filerun-next", "Next".to_owned()).primary().disabled(!self.can_leave_source()).on_click(cx.listener(|this, _, window, cx| this.next(window, cx))).into_any_element()),
                Step::Settings => buttons.push(button("filerun-next", "Next".to_owned()).primary().on_click(cx.listener(|this, _, window, cx| this.next(window, cx))).into_any_element()),
                Step::Run => buttons.push(button("filerun-start", "Run".to_owned()).primary().on_click(cx.listener(|this, _, window, cx| this.begin_run(window, cx))).into_any_element()),
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
            .child(div().debug_selector(|| "filerun-message".into()).flex_1().min_w_0().truncate().text_size(px(11.5)).text_color(theme::danger()).children(self.message.clone()))
            .children(buttons)
    }
}

/// One line of the list of failed and skipped statements.
fn entry_row(index: usize, entry: &Entry) -> AnyElement {
    let color = if entry.kind == EntryKind::Failed { theme::danger() } else { theme::text_muted() };
    div()
        .id(("filerun-entry", index))
        .debug_selector(move || format!("filerun-entry-{index}"))
        .h(px(ENTRY_HEIGHT))
        .px(px(8.0))
        .flex()
        .items_center()
        .truncate()
        .text_size(px(11.5))
        .text_color(color)
        .child(entry.text())
        .into_any_element()
}

/// The numbers of a finished run.
fn stats_line(report: &Report) -> String {
    let mut parts = vec![
        format!("{} ok", grouped(report.ok)),
        format!("{} failed", grouped(report.failed)),
        format!("{} skipped", grouped(report.skipped)),
    ];
    if report.passed_over > 0 {
        parts.push(format!("{} left out before the start", grouped(report.passed_over)));
    }
    if report.replayed > 0 {
        parts.push(format!("{} run again to set the session up", grouped(report.replayed)));
    }
    if report.not_repeated > 0 {
        parts.push(format!("{} SET statements before the start not repeated", grouped(report.not_repeated)));
    }
    parts.push(format!("last statement #{}", grouped(report.last)));
    if report.affected > 0 {
        parts.push(format!("{} rows changed", grouped(report.affected)));
    }
    if report.result_sets > 0 {
        parts.push(format!("{} result sets discarded", grouped(report.result_sets)));
    }
    parts.push(format!("{} read in {}", human_bytes(report.bytes), format_duration(report.elapsed)));
    parts.push(format!("{} statements/s", grouped(report.statements_per_second() as u64)));
    parts.join(" \u{00B7} ")
}

impl Render for FileRunView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tab_id = self.tab_id;
        let panel = match self.step {
            Step::Source => self.render_source().into_any_element(),
            Step::Settings => self.render_settings(cx).into_any_element(),
            Step::Run => self.render_run(cx).into_any_element(),
        };
        div()
            .id(("filerun-view", tab_id))
            .debug_selector(|| "filerun-view".into())
            .size_full()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(theme::background())
            .text_size(px(12.0))
            .text_color(theme::text())
            .child(self.render_header(cx))
            .child(self.render_steps())
            .child(panel)
            .child(self.render_footer(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::file_run::run::Report;

    #[test]
    fn the_stats_name_what_ran_what_was_left_out_and_where_it_ended() {
        let mut report = Report::failed(String::new(), std::time::Duration::from_secs(2));
        report.outcome = Outcome::Completed;
        (report.ok, report.failed, report.skipped, report.passed_over, report.last, report.bytes) = (1500, 2, 3, 40, 1545, 2048);
        let line = stats_line(&report);
        for part in ["1,500 ok", "2 failed", "3 skipped", "40 left out before the start", "last statement #1,545", "2.0 KB read in 2.00 s", "751 statements/s"] {
            assert!(line.contains(part), "{part}: {line}");
        }
    }
}
