//! How the import wizard looks: the header and the steps, one panel per step and the footer with the
//! buttons. Everything standard is a `gpui-component` control (`Select`, `Input`, `Checkbox`, `Button`, the
//! caret menu, `Progress`); the preview is the app's result grid, embedded.

use gpui::{Context, Window};
use gpui_component::progress::Progress;
use gpui_component::select::Select;

use super::super::*;
use super::run::{LoadSettings, Outcome, ERROR_LOG_CAP};
use super::view::{grouped, human_bytes, FormatChoice, ImportView, SampleState, Step, TargetsState};

const ROW_HEIGHT: f32 = 34.0;
const ERROR_ROW_HEIGHT: f32 = 22.0;

/// A labelled field of the option panels.
fn field(label: &'static str, width: f32, control: impl IntoElement) -> Div {
    div().flex().flex_col().gap(px(3.0)).w(px(width)).child(div().text_size(px(11.0)).text_color(theme::text_muted()).child(label)).child(control)
}

impl ImportView {
    fn target_label(&self) -> String {
        let place = if self.origin.schema.is_empty() { self.origin.database.clone() } else { self.origin.schema.clone() };
        if self.new_table && !self.created {
            format!("a new table in {place}")
        } else if self.origin.schema.is_empty() {
            self.origin.object.clone()
        } else {
            format!("{}.{}", self.origin.schema, self.origin.object)
        }
    }

    fn file_label(&self) -> String {
        self.path.as_ref().and_then(|path| path.file_name()).map_or_else(|| "no file chosen".to_owned(), |name| name.to_string_lossy().into_owned())
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tab_id = self.tab_id;
        let detail = match &self.sample {
            SampleState::Ready(sample) => format!(
                "{} · {} · {} columns · {}",
                self.file_label(),
                human_bytes(sample.file_size),
                sample.columns.len(),
                if sample.more { format!("preview of the first {}", super::tab::PREVIEW_ROWS) } else { format!("{} rows", sample.rows.len()) }
            ),
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
            .child(Icon::empty().path("icons/folder-input.svg").size(px(16.0)).text_color(theme::accent()))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(div().debug_selector(|| "import-title".into()).truncate().text_size(px(13.0)).child(format!("Import data into {}", self.target_label())))
                    .child(div().debug_selector(|| "import-file".into()).truncate().text_size(px(11.0)).text_color(theme::text_muted()).child(detail)),
            )
            .child(
                Button::new(("import-browse", tab_id))
                    .debug_selector(|| "import-browse".into())
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
                .debug_selector(move || format!("import-step-{}", label.to_lowercase()))
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
            .child(step("2", "Mapping", Step::Mapping))
            .child(step("3", "Load", Step::Load))
    }

    // ---- 1. Source -----------------------------------------------------------------------------------------

    fn render_source(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tab_id = self.tab_id;
        let json = self.format == FormatChoice::Json;
        let custom = self.format == FormatChoice::Custom;
        let toggle = |id: &'static str, label: &'static str, checked: bool, apply: fn(&mut ImportView, bool)| {
            div().debug_selector(move || id.into()).pt(px(16.0)).child(Checkbox::new((id, tab_id)).label(label).checked(checked).on_click(cx.listener(move |this, checked: &bool, window, cx| {
                apply(this, *checked);
                this.options_changed(window, cx);
            })))
        };
        let status = match &self.sample {
            SampleState::NoFile => Some(("Choose the file to import.".to_owned(), theme::text_muted())),
            SampleState::Reading => Some(("Reading the file\u{2026}".to_owned(), theme::text_muted())),
            SampleState::Failed(message) => Some((message.clone(), theme::danger())),
            SampleState::Ready(sample) if !sample.bad.is_empty() => Some((
                format!("{} of the first rows cannot be read (for example row {} {}). The load applies the error policy you choose to rows like that.", sample.bad.len(), sample.bad[0].number, sample.bad[0].problem),
                theme::warning(),
            )),
            SampleState::Ready(_) => None,
        };
        div()
            .debug_selector(|| "import-source-panel".into())
            .flex_shrink_0()
            .px(px(12.0))
            .py(px(10.0))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(10.0))
                    .child(field("Format", 230.0, div().debug_selector(|| "import-format".into()).child(Select::new(&self.format_select).small().w_full())))
                    .child(field("Encoding", 230.0, div().debug_selector(|| "import-encoding".into()).child(Select::new(&self.encoding_select).small().w_full())))
                    .when(custom, |this| this.child(field("Delimiter", 70.0, div().debug_selector(|| "import-delimiter".into()).child(Input::new(&self.delimiter).small()))))
                    .when(!json, |this| {
                        this.child(field("Quote", 60.0, div().debug_selector(|| "import-quote".into()).child(Input::new(&self.quote).small())))
                            .child(field("Escape", 100.0, div().debug_selector(|| "import-escape".into()).child(Input::new(&self.escape).small())))
                            .child(field("Skip lines", 70.0, div().debug_selector(|| "import-skip".into()).child(Input::new(&self.skip_lines).small())))
                            .child(toggle("import-header", "First row is the header", self.header, |this, checked| this.header = checked))
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(10.0))
                    .child(field("NULL text", 120.0, div().debug_selector(|| "import-null-text".into()).child(Input::new(&self.null_text).small())))
                    .child(toggle("import-empty-null", "Empty is NULL", self.empty_null, |this, checked| this.empty_null = checked))
                    .child(toggle("import-trim", "Trim spaces", self.trim, |this, checked| this.trim = checked))
                    .child(field("Numbers", 150.0, div().debug_selector(|| "import-decimal".into()).child(Select::new(&self.decimal_select).small().w_full())))
                    .child(field("Date format", 130.0, div().debug_selector(|| "import-date-format".into()).child(Input::new(&self.date_format).small())))
                    .child(field("Timestamp format", 170.0, div().debug_selector(|| "import-timestamp-format".into()).child(Input::new(&self.timestamp_format).small()))),
            )
            .children(status.map(|(text, color)| div().debug_selector(|| "import-source-status".into()).text_size(px(11.5)).text_color(color).child(text)))
    }

    // ---- 2. Mapping ----------------------------------------------------------------------------------------

    fn render_mapping(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let intro = if self.new_table {
            "A new table is created from the columns of the file. Name them, change their types, leave out what you do not need."
        } else {
            "Each column of the file goes to a column of the table, or is skipped. Columns with the same name are matched already."
        };
        let general: Vec<String> = self.notes.iter().filter(|note| note.source.is_none()).map(|note| note.message.clone()).collect();
        let body = match (&self.targets, self.ready_sample()) {
            (TargetsState::Loading, _) => div().p(px(12.0)).text_color(theme::text_muted()).child("Reading the columns of the table\u{2026}").into_any_element(),
            (TargetsState::Failed(message), _) => div().debug_selector(|| "import-targets-error".into()).p(px(12.0)).text_color(theme::danger()).child(message.clone()).into_any_element(),
            (_, None) => div().p(px(12.0)).text_color(theme::text_muted()).child("Choose a file first.").into_any_element(),
            (_, Some(sample)) => {
                let rows = if self.new_table { self.new_columns.len() } else { sample.columns.len() };
                uniform_list(
                    "import-mapping-rows",
                    rows,
                    cx.processor(|this, range: Range<usize>, window, cx| range.map(|index| this.mapping_row(index, window, cx)).collect::<Vec<_>>()),
                )
                .flex_1()
                .min_h_0()
                .track_scroll(&self.mapping_scroll)
                .into_any_element()
            }
        };
        div()
            .debug_selector(|| "import-mapping-panel".into())
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(div().px(px(12.0)).pt(px(8.0)).pb(px(4.0)).text_size(px(11.5)).text_color(theme::text_muted()).child(intro))
            .when(self.new_table, |this| {
                this.child(
                    div().px(px(12.0)).pb(px(6.0)).flex().items_center().gap(px(8.0)).child(div().w(px(80.0)).text_size(px(11.5)).child("Table name")).child(
                        div().debug_selector(|| "import-new-name".into()).w(px(280.0)).child(Input::new(&self.new_name).small().disabled(self.created)),
                    ).when(self.created, |this| this.child(div().text_size(px(11.5)).text_color(theme::success()).child("created"))),
                )
            })
            .child(div().flex_1().min_h_0().flex().flex_col().child(body))
            .children((!general.is_empty()).then(|| {
                div().flex_shrink_0().px(px(12.0)).py(px(4.0)).flex().flex_col().gap(px(2.0)).children(general.into_iter().enumerate().map(|(index, message)| {
                    div().debug_selector(move || format!("import-note-general-{index}")).text_size(px(11.5)).text_color(theme::warning()).child(message)
                }))
            }))
    }

    /// One row of the mapping, for the column `index` of the file.
    fn mapping_row(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let tab_id = self.tab_id;
        let sample = self.ready_sample();
        let source = sample.as_ref().and_then(|sample| sample.columns.get(index).cloned()).unwrap_or_default();
        let note = self.notes.iter().find(|note| note.source == Some(index)).map(|note| note.message.clone());
        let name = div().w(px(150.0)).flex_shrink_0().truncate().text_size(px(12.0)).child(source);
        let note_cell = div().debug_selector(move || format!("import-map-note-{index}")).flex_1().min_w_0().truncate().text_size(px(11.0)).text_color(theme::warning()).children(note);
        let row = div().id(("import-map-row", index)).h(px(ROW_HEIGHT)).px(px(12.0)).flex().items_center().gap(px(8.0));
        if self.new_table {
            let (Some(column), Some((name_input, type_input))) = (self.new_columns.get(index).map(|column| column.include), self.ensure_new_inputs(index, window, cx)) else {
                return row.into_any_element();
            };
            let (include, locked) = (column, self.created);
            let types: Vec<&'static str> = self.dialect.common_types().to_vec();
            let type_menu_input = type_input.clone();
            return row
                .child(div().debug_selector(move || format!("import-include-{index}")).child(
                    Checkbox::new(gpui::ElementId::Name(format!("import-include-{tab_id}-{index}").into())).checked(include).disabled(locked).on_click(cx.listener(move |this, checked: &bool, _, cx| this.set_include(index, *checked, cx))),
                ))
                .child(name)
                .child(div().debug_selector(move || format!("import-new-column-{index}")).w(px(170.0)).child(Input::new(&name_input).small().disabled(locked || !include)))
                .child(
                    div().flex().items_center().gap(px(2.0)).child(div().debug_selector(move || format!("import-new-type-{index}")).w(px(150.0)).child(Input::new(&type_input).small().disabled(locked || !include))).child(
                        Button::new(gpui::ElementId::Name(format!("import-new-types-{tab_id}-{index}").into()))
                            .icon(IconName::ChevronDown)
                            .ghost()
                            .compact()
                            .disabled(locked || !include)
                            .tooltip("Common types")
                            .dropdown_menu(move |menu, _, _| {
                                types.iter().fold(menu, |menu, &data_type| {
                                    let type_input = type_menu_input.clone();
                                    menu.item(PopupMenuItem::new(data_type).on_click(move |_, window, cx| type_input.update(cx, |state, cx| state.set_value(data_type, window, cx))))
                                })
                            }),
                    ),
                )
                .child(note_cell)
                .into_any_element();
        }
        let Some(select) = self.ensure_map_select(index, window, cx) else { return row.into_any_element() };
        let target_type = self
            .assignments
            .get(index)
            .copied()
            .flatten()
            .and_then(|target| self.targets().get(target).map(|target| target.data_type.clone()))
            .unwrap_or_default();
        row.child(name)
            .child(Icon::new(IconName::ArrowRight).size(px(12.0)).text_color(theme::text_faint()))
            .child(div().debug_selector(move || format!("import-map-{index}")).w(px(240.0)).child(Select::new(&select).small().w_full().menu_max_h(px(260.0))))
            .child(div().debug_selector(move || format!("import-map-type-{index}")).w(px(130.0)).flex_shrink_0().truncate().text_size(px(11.0)).text_color(theme::type_text()).child(target_type))
            .child(note_cell)
            .into_any_element()
    }

    // ---- 3. Load -------------------------------------------------------------------------------------------

    fn render_load(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let body = if let Some(run) = &self.run {
            self.render_progress(run).into_any_element()
        } else if let Some(report) = &self.report {
            self.render_summary(report, cx).into_any_element()
        } else {
            self.render_settings().into_any_element()
        };
        div().debug_selector(|| "import-load-panel".into()).flex_1().min_h_0().flex().flex_col().child(body)
    }

    fn render_settings(&self) -> impl IntoElement {
        let mapped = self.mapped_count();
        let columns = match self.ready_sample() {
            Some(sample) => sample.columns.len(),
            None => 0,
        };
        let problems = self.notes.len();
        let transactions = self.transactions();
        div()
            .p(px(12.0))
            .flex()
            .flex_col()
            .gap(px(12.0))
            .child(
                div().debug_selector(|| "import-load-summary".into()).text_size(px(12.5)).child(format!(
                    "{} will be loaded into {}: {mapped} of {columns} columns{}.",
                    self.file_label(),
                    self.target_label(),
                    if self.new_table && !self.created { " (the table is created first)" } else { "" }
                )),
            )
            .children((problems > 0).then(|| {
                div().debug_selector(|| "import-load-problems".into()).text_size(px(11.5)).text_color(theme::warning()).child(format!(
                    "{problems} possible problem{} found in the preview: see the Mapping step. Rows the server refuses follow the error policy below.",
                    if problems == 1 { "" } else { "s" }
                ))
            }))
            .when(self.new_table && !self.created, |this| {
                this.child(
                    div().flex().items_center().gap(px(10.0)).child(
                        div().text_size(px(12.0)).text_color(theme::text_muted()).child("The table does not exist yet: its SQL is shown for review, and nothing runs until you apply it."),
                    ),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(12.0))
                    .child(field("Rows per batch", 130.0, div().debug_selector(|| "import-batch".into()).child(Input::new(&self.batch_size).small())))
                    .child(field("Transactions", 210.0, div().debug_selector(|| "import-transaction".into()).child(Select::new(&self.transaction_select).small().w_full().disabled(!transactions))))
                    .child(field("When a row is refused", 230.0, div().debug_selector(|| "import-on-error".into()).child(Select::new(&self.on_error_select).small().w_full()))),
            )
            .child(div().debug_selector(|| "import-transaction-note".into()).text_size(px(11.5)).text_color(theme::text_muted()).child(
                self.no_transactions_note().unwrap_or_else(|| {
                    "Commit every batch keeps the batches already loaded if the import stops. One transaction keeps nothing unless the whole file loads.".to_owned()
                }),
            ))
            .child(div().text_size(px(11.5)).text_color(theme::text_muted()).child(format!(
                "Rows are inserted in batches of up to {} through the connection's driver, on a session of their own.",
                grouped(LoadSettings::MAX_BATCH as u64)
            )))
    }

    fn render_progress(&self, run: &super::view::RunningLoad) -> impl IntoElement {
        let snapshot = run.last;
        let seconds = run.started.elapsed().as_secs_f64();
        let rate = if seconds > 0.0 { snapshot.inserted as f64 / seconds } else { 0.0 };
        let fraction = if run.total_bytes > 0 { (snapshot.bytes as f32 / run.total_bytes as f32).clamp(0.0, 1.0) } else { 0.0 };
        div()
            .p(px(12.0))
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(div().text_size(px(13.0)).child(if run.cancelling { "Cancelling\u{2026}" } else { "Importing\u{2026}" }))
            .child(div().debug_selector(|| "import-progress-bar".into()).w_full().child(Progress::new(("import-bar", self.tab_id)).value(fraction * 100.0)))
            .child(
                div().debug_selector(|| "import-progress".into()).flex().flex_col().gap(px(3.0)).text_size(px(12.0)).text_color(theme::text_muted())
                    .child(format!("{} rows read, {} inserted, {} skipped", grouped(snapshot.rows_read), grouped(snapshot.inserted), grouped(snapshot.skipped)))
                    .child(format!("{} of {} read \u{00B7} {} rows/s \u{00B7} {:.0} s", human_bytes(snapshot.bytes), human_bytes(run.total_bytes), grouped(rate as u64), seconds)),
            )
    }

    fn render_summary(&self, report: &super::run::Report, cx: &mut Context<Self>) -> impl IntoElement {
        let (headline, color) = match &report.outcome {
            Outcome::Completed if report.errors_total == 0 => (format!("Imported {} rows into {}", grouped(report.inserted), self.target_label()), theme::success()),
            Outcome::Completed => (format!("Imported {} rows into {}, skipped {}", grouped(report.inserted), self.target_label(), grouped(report.errors_total)), theme::warning()),
            Outcome::Stopped(reason) => (format!("Stopped: {reason}"), theme::danger()),
            Outcome::Cancelled => ("Cancelled".to_owned(), theme::warning()),
            Outcome::Failed(message) => (format!("Failed: {message}"), theme::danger()),
        };
        let kept = match (&report.outcome, report.rolled_back) {
            (_, rolled) if rolled > 0 => format!("The transaction was rolled back: none of the {} rows loaded before the stop were kept.", grouped(rolled)),
            (Outcome::Completed, _) => String::new(),
            _ if report.uncertain > 0 && report.inserted == 0 => String::new(),
            _ if report.inserted > 0 => format!("{} rows were already committed and stay in the table.", grouped(report.inserted)),
            _ => "No rows were added.".to_owned(),
        };
        let errors = report.errors.clone();
        let more = report.errors_total.saturating_sub(errors.len() as u64);
        div()
            .debug_selector(|| "import-summary".into())
            .flex_1()
            .min_h_0()
            .p(px(12.0))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(div().debug_selector(|| "import-headline".into()).text_size(px(13.0)).text_color(color).child(headline))
            .children((!kept.is_empty()).then(|| div().debug_selector(|| "import-kept".into()).text_size(px(12.0)).child(kept)))
            .child(div().debug_selector(|| "import-stats".into()).text_size(px(12.0)).text_color(theme::text_muted()).child(stats_line(report)))
            .children((report.uncertain > 0).then(|| {
                div().debug_selector(|| "import-uncertain".into()).text_size(px(12.0)).text_color(theme::warning()).child(format!(
                    "{} rows may or may not have been saved: check the table before importing again.",
                    grouped(report.uncertain)
                ))
            }))
            .children((!errors.is_empty()).then(|| {
                let count = errors.len();
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(div().text_size(px(11.5)).text_color(theme::text_muted()).child(format!(
                        "Rows left out{}:",
                        if more > 0 { format!(" (the first {ERROR_LOG_CAP} of {})", grouped(report.errors_total)) } else { String::new() }
                    )))
                    .child(
                        div().debug_selector(|| "import-errors".into()).flex_1().min_h_0().border_1().border_color(theme::border()).rounded(px(4.0)).child(
                            uniform_list(
                                "import-error-rows",
                                count,
                                cx.processor(move |this, range: Range<usize>, _window, _cx| {
                                    range
                                        .filter_map(|index| {
                                            let error = this.report.as_ref()?.errors.get(index)?;
                                            Some(
                                                div()
                                                    .id(("import-error", index))
                                                    .debug_selector(move || format!("import-error-{index}"))
                                                    .h(px(ERROR_ROW_HEIGHT))
                                                    .px(px(8.0))
                                                    .flex()
                                                    .items_center()
                                                    .truncate()
                                                    .text_size(px(11.5))
                                                    .child(error.to_string())
                                                    .into_any_element(),
                                            )
                                        })
                                        .collect::<Vec<_>>()
                                }),
                            )
                            .size_full()
                            .track_scroll(&self.errors_scroll),
                        ),
                    )
            }))
    }

    // ---- the buttons ---------------------------------------------------------------------------------------

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tab_id = self.tab_id;
        let button = |id: &'static str, label: &'static str| Button::new((id, tab_id)).debug_selector(move || id.into()).label(label).compact();
        let mut buttons: Vec<AnyElement> = Vec::new();
        if self.run.is_some() {
            let cancelling = self.run.as_ref().is_some_and(|run| run.cancelling);
            buttons.push(button("import-cancel", "Cancel import").danger().disabled(cancelling).on_click(cx.listener(|this, _, _, cx| this.cancel(cx))).into_any_element());
        } else if self.report.is_some() {
            buttons.push(button("import-again", "Import another file").ghost().on_click(cx.listener(|this, _, window, cx| this.again(window, cx))).into_any_element());
            buttons.push(button("import-close", "Close").primary().on_click(cx.listener(|this, _, window, cx| this.close_tab(window, cx))).into_any_element());
        } else {
            if self.step != Step::Source {
                buttons.push(button("import-back", "Back").ghost().on_click(cx.listener(|this, _, _, cx| this.back(cx))).into_any_element());
            }
            match self.step {
                Step::Source => buttons.push(button("import-next", "Next").primary().disabled(!self.can_leave_source()).on_click(cx.listener(|this, _, window, cx| this.next(window, cx))).into_any_element()),
                Step::Mapping => buttons.push(button("import-next", "Next").primary().disabled(!self.can_leave_mapping()).on_click(cx.listener(|this, _, window, cx| this.next(window, cx))).into_any_element()),
                Step::Load if self.new_table && !self.created => {
                    buttons.push(button("import-create", "Create table\u{2026}").primary().on_click(cx.listener(|this, _, window, cx| this.create_table(window, cx))).into_any_element())
                }
                Step::Load => buttons.push(button("import-start", "Import").primary().on_click(cx.listener(|this, _, window, cx| this.begin_load(window, cx))).into_any_element()),
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
            .child(div().debug_selector(|| "import-message".into()).flex_1().min_w_0().truncate().text_size(px(11.5)).text_color(theme::danger()).children(self.message.clone()))
            .children(buttons)
    }
}

/// The numbers of a finished load: what is in the table, and (apart) what a rollback took out again.
fn stats_line(report: &super::run::Report) -> String {
    format!(
        "{} rows read \u{00B7} {} inserted \u{00B7} {}{} skipped \u{00B7} {} read in {:.1} s \u{00B7} {} rows/s",
        grouped(report.rows_read),
        grouped(report.inserted),
        if report.rolled_back > 0 { format!("{} rolled back \u{00B7} ", grouped(report.rolled_back)) } else { String::new() },
        grouped(report.errors_total),
        human_bytes(report.bytes),
        report.elapsed.as_secs_f64(),
        grouped(report.rows_per_second() as u64)
    )
}

impl Render for ImportView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.describe_pending) {
            self.load_targets(window, cx);
        }
        let tab_id = self.tab_id;
        let preview = (self.step != Step::Load && self.run.is_none() && matches!(self.sample, SampleState::Ready(_)))
            .then(|| self.root.update(cx, |root, root_cx| root.embed_tab_results(tab_id, root_cx)).ok())
            .flatten();
        let panel = match self.step {
            Step::Source => self.render_source(cx).into_any_element(),
            Step::Mapping => self.render_mapping(cx).into_any_element(),
            Step::Load => self.render_load(cx).into_any_element(),
        };
        let preview_height = if self.step == Step::Mapping { Some(px(190.0)) } else { None };
        div()
            .id(("import-view", tab_id))
            .debug_selector(|| "import-view".into())
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
            .children(preview.map(|preview| {
                let frame = div()
                    .debug_selector(|| "import-preview".into())
                    .min_h(px(120.0))
                    .flex()
                    .flex_col()
                    .border_t_1()
                    .border_color(theme::border())
                    .child(ui::section_header("Preview"))
                    .child(div().flex_1().min_h_0().flex().child(preview));
                match preview_height {
                    Some(height) => frame.h(height).flex_shrink_0(),
                    None => frame.flex_1(),
                }
            }))
            .child(self.render_footer(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::data_import::run::Report;

    fn report(inserted: u64, rolled_back: u64) -> Report {
        Report {
            outcome: Outcome::Completed,
            inserted,
            rolled_back,
            skipped: 0,
            rows_read: 1500,
            errors: Vec::new(),
            errors_total: 0,
            uncertain: 0,
            bytes: 2048,
            elapsed: std::time::Duration::from_secs(1),
        }
    }

    #[test]
    fn the_stats_say_inserted_for_what_stays_and_rolled_back_for_what_does_not() {
        let kept = stats_line(&report(1500, 0));
        assert!(kept.contains("1,500 inserted") && !kept.contains("rolled back"), "{kept}");
        let undone = stats_line(&report(0, 1200));
        assert!(undone.contains("0 inserted") && undone.contains("1,200 rolled back"), "{undone}");
        assert!(!undone.contains("1,200 inserted"), "{undone}");
    }
}
