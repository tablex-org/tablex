//! How the transfer wizard looks: the header and the steps, one panel per step and the footer with the buttons.
//! Everything standard is a `gpui-component` control (`Select`, `Input`, `Textarea`, `Checkbox`, `Button`, the caret
//! menu, `Progress`); every list of rows (the tables, the mapping, the results, the errors) is virtualized, and the
//! widgets of a row exist for the table that is selected and the rows on screen only.

use gpui::{Context, SharedString, Window};
use gpui_component::progress::Progress;
use gpui_component::select::Select;

use super::super::data_import::run::{Outcome, ERROR_LOG_CAP};
use super::super::*;
use super::job::{FinishedLine, TransferOutcome};
use super::model::{Action, Loaded};
use super::sink::FileFormat;
use super::reader::{pins_session_zone, SnapshotState, QUERY_UTC_NOTE};
use super::view::{grouped, human_bytes, plural, SourceMode, Step, TargetKind, TransferView};

const TABLE_ROW_HEIGHT: f32 = 28.0;
const JOB_ROW_HEIGHT: f32 = 30.0;
const COLUMN_ROW_HEIGHT: f32 = 34.0;
const RESULT_ROW_HEIGHT: f32 = 26.0;
const ERROR_ROW_HEIGHT: f32 = 22.0;

/// A labelled field of the option panels.
fn field(label: &'static str, width: f32, control: impl IntoElement) -> Div {
    div().flex().flex_col().gap(px(3.0)).w(px(width)).child(div().text_size(px(11.0)).text_color(theme::text_muted()).child(label)).child(control)
}

/// What a query typed on a source whose session is set to UTC is told (a table is read as it is stored).
fn utc_query_note(view: &TransferView) -> Option<&'static str> {
    (view.mode == SourceMode::Query && pins_session_zone(view.source_kind)).then_some(QUERY_UTC_NOTE)
}

impl TransferView {
    fn source_label(&self) -> String {
        let place = if self.source.schema.is_empty() { self.source.database.clone() } else { self.source.schema.clone() };
        match self.mode {
            SourceMode::Query => "a query".to_owned(),
            SourceMode::Tables => format!("{} of {place}", match self.selected_tables() {
                1 => "1 table".to_owned(),
                count => format!("{count} tables"),
            }),
        }
    }

    fn render_header(&self) -> impl IntoElement {
        let target = match self.target_kind {
            TargetKind::Table => format!("tables of {}", self.target.label()),
            TargetKind::File => format!("{} files", self.format.label()),
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
            .child(Icon::empty().path("icons/arrow-left-right.svg").size(px(16.0)).text_color(theme::accent()))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(div().debug_selector(|| "transfer-title".into()).truncate().text_size(px(13.0)).child("Data transfer"))
                    .child(
                        div()
                            .debug_selector(|| "transfer-subtitle".into())
                            .truncate()
                            .text_size(px(11.0))
                            .text_color(theme::text_muted())
                            .child(if self.step == Step::Source { format!("From {}", self.source_label()) } else { format!("From {} to {target}", self.source_label()) }),
                    ),
            )
    }

    fn render_steps(&self) -> impl IntoElement {
        let step = |number: &'static str, label: &'static str, this: Step| {
            let current = self.step == this;
            div()
                .debug_selector(move || format!("transfer-step-{}", label.to_lowercase()))
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
            .child(step("2", "Target", Step::Target))
            .child(step("3", "Mapping", Step::Mapping))
            .child(step("4", "Run", Step::Run))
    }

    // ---- 1. Source -----------------------------------------------------------------------------------------

    fn render_source(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tab_id = self.tab_id;
        let body = match self.mode {
            SourceMode::Query => div()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .gap(px(6.0))
                .child(div().text_size(px(11.5)).text_color(theme::text_muted()).child("One SELECT. Its rows are read in pages on a session of their own, in read-only mode. Add an ORDER BY for a stable result."))
                .children(utc_query_note(self).map(|note| div().debug_selector(|| "transfer-query-utc".into()).text_size(px(11.5)).text_color(theme::text_muted()).child(note)))
                .child(div().debug_selector(|| "transfer-query".into()).child(Textarea::new(&self.query)))
                .into_any_element(),
            SourceMode::Tables => match &self.listing {
                Loaded::Pending => div().p(px(4.0)).text_color(theme::text_muted()).child("Reading the tables\u{2026}").into_any_element(),
                Loaded::Failed(message) => div().debug_selector(|| "transfer-tables-error".into()).p(px(4.0)).text_color(theme::danger()).child(message.clone()).into_any_element(),
                Loaded::Ready(()) if self.tables.is_empty() => {
                    div().debug_selector(|| "transfer-no-tables".into()).p(px(4.0)).text_color(theme::text_muted()).child("This schema has no tables.").into_any_element()
                }
                Loaded::Ready(()) => div()
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
                            .child(div().debug_selector(|| "transfer-filter".into()).w(px(260.0)).child(Input::new(&self.filter).small()))
                            .child(Button::new(("transfer-all", tab_id)).debug_selector(|| "transfer-select-all".into()).label("Select all").compact().ghost().on_click(cx.listener(|this, _, _, cx| this.select_shown(true, cx))))
                            .child(Button::new(("transfer-none", tab_id)).debug_selector(|| "transfer-select-none".into()).label("Clear").compact().ghost().on_click(cx.listener(|this, _, _, cx| this.select_shown(false, cx))))
                            .child(div().debug_selector(|| "transfer-selected-count".into()).text_size(px(11.5)).text_color(theme::text_muted()).child(format!("{} of {} selected", self.selected_tables(), self.tables.len()))),
                    )
                    .child(
                        div().flex_1().min_h_0().border_1().border_color(theme::border()).rounded(px(4.0)).child(
                            uniform_list(
                                "transfer-table-rows",
                                self.shown.len(),
                                cx.processor(|this, range: Range<usize>, _window, cx| {
                                    range.filter_map(|position| this.table_row(position, cx)).collect::<Vec<_>>()
                                }),
                            )
                            .size_full()
                            .track_scroll(&self.tables_scroll),
                        ),
                    )
                    .into_any_element(),
            },
        };
        div()
            .debug_selector(|| "transfer-source-panel".into())
            .flex_1()
            .min_h_0()
            .px(px(12.0))
            .py(px(10.0))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(field("What to transfer", 280.0, div().debug_selector(|| "transfer-mode".into()).child(Select::new(&self.mode_select).small().w_full())))
            .child(body)
    }

    fn table_row(&self, position: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        let table_index = *self.shown.get(position)?;
        let table = self.tables.get(table_index)?;
        let tab_id = self.tab_id;
        Some(
            div()
                .id(("transfer-table-row", position))
                .h(px(TABLE_ROW_HEIGHT))
                .px(px(8.0))
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(div().debug_selector(move || format!("transfer-table-{position}")).child(
                    Checkbox::new(gpui::ElementId::Name(format!("transfer-pick-{tab_id}-{table_index}").into())).checked(table.selected).on_click(cx.listener(move |this, checked: &bool, _, cx| this.set_selected(table_index, *checked, cx))),
                ))
                .child(div().flex_1().min_w_0().truncate().text_size(px(12.0)).child(table.name.clone()))
                .into_any_element(),
        )
    }

    // ---- 2. Target -----------------------------------------------------------------------------------------

    fn render_target(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tab_id = self.tab_id;
        let files = self.target_kind == TargetKind::File;
        div()
            .debug_selector(|| "transfer-target-panel".into())
            .flex_1()
            .min_h_0()
            .px(px(12.0))
            .py(px(10.0))
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(field("Write to", 280.0, div().debug_selector(|| "transfer-target-kind".into()).child(Select::new(&self.kind_select).small().w_full())))
            .when(!files, |this| {
                this.child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap(px(10.0))
                        .child(field("Connection", 280.0, div().debug_selector(|| "transfer-target-connection".into()).child(Select::new(&self.connection_select).small().w_full().menu_max_h(px(260.0)))))
                        .child(field("Database or schema", 280.0, div().debug_selector(|| "transfer-target-namespace".into()).child(Select::new(&self.namespace_select).small().w_full().menu_max_h(px(260.0))))),
                )
                .child(div().text_size(px(11.5)).text_color(theme::text_muted()).child(
                    "Only the databases and schemas the navigator has loaded are listed: expand a database in the navigator to see its schemas. A table that exists is appended to; one that does not is created, after you review its CREATE TABLE.",
                ))
            })
            .when(files, |this| {
                this.child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_end()
                        .gap(px(10.0))
                        .child(field("Format", 200.0, div().debug_selector(|| "transfer-format".into()).child(Select::new(&self.format_select).small().w_full())))
                        .when(self.format == FileFormat::SqlInsert, |this| {
                            this.child(field("SQL dialect", 200.0, div().debug_selector(|| "transfer-sql-dialect".into()).child(Select::new(&self.sql_dialect_select).small().w_full())))
                        }),
                )
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_end()
                        .gap(px(10.0))
                        .child(field("Folder", 460.0, div().debug_selector(|| "transfer-folder".into()).child(Input::new(&self.folder).small())))
                        .child(Button::new(("transfer-browse", tab_id)).debug_selector(|| "transfer-browse".into()).label("Choose folder\u{2026}").compact().ghost().on_click(cx.listener(|this, _, window, cx| this.browse(window, cx)))),
                )
                .when(self.single_file(), |this| {
                    this.child(field("File name", 460.0, div().debug_selector(|| "transfer-file-name".into()).child(Input::new(&self.file_name).small())))
                })
                .child(div().debug_selector(|| "transfer-overwrite".into()).child(
                    Checkbox::new(("transfer-overwrite-check", tab_id)).label("Replace existing files").checked(self.overwrite).on_click(cx.listener(|this, checked: &bool, _, cx| this.set_overwrite(*checked, cx))),
                ))
                .child(div().text_size(px(11.5)).text_color(theme::text_muted()).child(
                    "Rows are written as they are read, in pages: a table of any size costs a buffer of memory. CSV follows the Export settings (delimiter, header, NULL text). A file is complete when it appears; a stopped export leaves nothing.",
                ))
            })
    }

    // ---- 3. Mapping ----------------------------------------------------------------------------------------

    fn render_mapping(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let files = self.target_kind == TargetKind::File;
        let intro = if files {
            "These files are written. Leave a table out by clearing its box."
        } else {
            "Each table goes to a table of the target: one that exists gets the rows of the columns with the same name, one that does not is created. Select a table to see and change its columns."
        };
        let general = {
            let duplicates = super::model::duplicate_targets(&self.jobs);
            let mut lines: Vec<String> = duplicates.into_iter().map(|name| format!("Two tables go to \"{name}\".")).collect();
            lines.extend(self.jobs.iter().filter(|job| self.onto_itself(job)).map(|job| self.onto_itself_message(job)));
            lines
        };
        div()
            .debug_selector(|| "transfer-mapping-panel".into())
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(div().px(px(12.0)).pt(px(8.0)).pb(px(4.0)).text_size(px(11.5)).text_color(theme::text_muted()).child(intro))
            .child(
                div().mx(px(12.0)).h(px(if files { 260.0 } else { 150.0 })).flex_shrink_0().when(files, |this| this.flex_1()).border_1().border_color(theme::border()).rounded(px(4.0)).child(
                    uniform_list(
                        "transfer-job-rows",
                        self.jobs.len(),
                        cx.processor(|this, range: Range<usize>, _window, cx| range.map(|index| this.job_row(index, cx)).collect::<Vec<_>>()),
                    )
                    .size_full()
                    .track_scroll(&self.jobs_scroll),
                ),
            )
            .when(!files, |this| this.child(self.render_job_detail(cx)))
            .children((!general.is_empty()).then(|| {
                // A few lines, then how many more: a schema of hundreds of tables must not push the columns off the panel.
                let mut general = general;
                if general.len() > 3 {
                    let more = general.len() - 3;
                    general.truncate(3);
                    general.push(format!("\u{2026}and {more} more."));
                }
                div().flex_shrink_0().px(px(12.0)).py(px(4.0)).flex().flex_col().gap(px(2.0)).children(general.into_iter().enumerate().map(|(index, message)| {
                    div().debug_selector(move || format!("transfer-note-general-{index}")).text_size(px(11.5)).text_color(theme::warning()).child(message)
                }))
            }))
    }

    /// One row of the list of tables: whether it is transferred, where it goes, and how its reading stands.
    fn job_row(&mut self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let tab_id = self.tab_id;
        let files = self.target_kind == TargetKind::File;
        let Some(job) = self.jobs.get(index) else { return div().into_any_element() };
        let selected = index == self.selected;
        let status = match (&job.columns, job.action) {
            (Loaded::Pending, _) => ("reading the columns\u{2026}".to_owned(), theme::text_muted()),
            (Loaded::Failed(message), _) => (message.clone(), theme::danger()),
            (_, Action::Leave) => ("left out".to_owned(), theme::text_faint()),
            (_, _) if files => (format!("{} columns", job.column_count()), theme::text_muted()),
            (_, Action::Create) => (format!("create \u{00B7} {} columns", job.mapped_count()), theme::accent()),
            (_, Action::Append) => match &job.target {
                Loaded::Pending => ("reading the table\u{2026}".to_owned(), theme::text_muted()),
                Loaded::Failed(message) => (message.clone(), theme::danger()),
                Loaded::Ready(_) => {
                    let notes = job.notes().len();
                    (
                        format!("append \u{00B7} {} of {} columns{}", job.mapped_count(), job.column_count(), if notes > 0 { format!(" \u{00B7} {notes} to check") } else { String::new() }),
                        if notes > 0 { theme::warning() } else { theme::success() },
                    )
                }
            },
        };
        let destination = if files {
            self.file_labels(cx).get(index).cloned().unwrap_or_default()
        } else {
            job.target_name.clone()
        };
        let (label, active) = (job.label.clone(), job.active());
        let row = div()
            .id(("transfer-job-row", index))
            .debug_selector(move || format!("transfer-job-{index}"))
            .w_full()
            .h(px(JOB_ROW_HEIGHT))
            .px(px(8.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .when(selected && !files, |this| this.bg(theme::selected()))
            .on_click(cx.listener(move |this, _, _, cx| this.select_job(index, cx)));
        row.child(div().debug_selector(move || format!("transfer-job-include-{index}")).child(
            Checkbox::new(gpui::ElementId::Name(format!("transfer-include-{tab_id}-{index}").into())).checked(active).on_click(cx.listener(move |this, checked: &bool, _, cx| this.toggle_job(index, *checked, cx))),
        ))
        .child(div().w(px(200.0)).flex_shrink_0().truncate().text_size(px(12.0)).child(label))
        .child(Icon::new(IconName::ArrowRight).size(px(12.0)).text_color(theme::text_faint()))
        .child(div().w(px(200.0)).flex_shrink_0().truncate().text_size(px(12.0)).child(destination))
        .child(div().debug_selector(move || format!("transfer-job-status-{index}")).flex_1().min_w_0().truncate().text_size(px(11.0)).text_color(status.1).child(status.0))
        .into_any_element()
    }

    /// The names of the files, one per job (a left-out job has an empty name).
    fn file_labels(&self, cx: &gpui::App) -> Vec<String> {
        let folder = std::path::PathBuf::from(self.folder.read(cx).value().trim());
        let paths = self.file_paths(&folder, cx);
        (0..self.jobs.len()).map(|index| paths.iter().find(|(job, _)| *job == index).and_then(|(_, path)| path.file_name()).map(|name| name.to_string_lossy().into_owned()).unwrap_or_default()).collect()
    }

    fn render_job_detail(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(job) = self.jobs.get(self.selected) else {
            return div().flex_1().into_any_element();
        };
        let body = match (&job.columns, job.action) {
            (Loaded::Pending, _) => div().p(px(12.0)).text_color(theme::text_muted()).child("Reading the columns\u{2026}").into_any_element(),
            (Loaded::Failed(message), _) => div().debug_selector(|| "transfer-columns-error".into()).p(px(12.0)).text_color(theme::danger()).child(message.clone()).into_any_element(),
            (_, Action::Leave) => div().p(px(12.0)).text_color(theme::text_muted()).child("This table is left out of the transfer.").into_any_element(),
            (_, Action::Append) if matches!(job.target, Loaded::Pending) => div().p(px(12.0)).text_color(theme::text_muted()).child("Reading the table\u{2026}").into_any_element(),
            (_, Action::Append) if matches!(job.target, Loaded::Failed(_)) => {
                let Loaded::Failed(message) = &job.target else { return div().into_any_element() };
                div().debug_selector(|| "transfer-target-error".into()).p(px(12.0)).text_color(theme::danger()).child(message.clone()).into_any_element()
            }
            (_, action) => {
                let rows = if action == Action::Create { job.new_columns.len() } else { job.column_count() };
                uniform_list(
                    "transfer-column-rows",
                    rows,
                    cx.processor(|this, range: Range<usize>, window, cx| range.map(|index| this.column_row(index, window, cx)).collect::<Vec<_>>()),
                )
                .flex_1()
                .min_h_0()
                .track_scroll(&self.columns_scroll)
                .into_any_element()
            }
        };
        let notes: Vec<String> = job.notes();
        let (action, name_input, action_select) = (job.action, self.widgets.get(&self.selected).map(|widgets| widgets.name.clone()), self.widgets.get(&self.selected).map(|widgets| widgets.action.clone()));
        let engine = job.target.ready().and_then(|target| target.engine.clone());
        div()
            .debug_selector(|| "transfer-detail".into())
            .flex_1()
            .min_h(px(120.0))
            .flex()
            .flex_col()
            .child(
                div()
                    .px(px(12.0))
                    .py(px(6.0))
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .child(div().text_size(px(11.5)).text_color(theme::text_muted()).child("Table"))
                    .children(name_input.map(|state| div().debug_selector(|| "transfer-job-name".into()).w(px(240.0)).child(Input::new(&state).small().disabled(action == Action::Leave))))
                    .children(action_select.map(|state| div().debug_selector(|| "transfer-job-action".into()).w(px(200.0)).child(Select::new(&state).small().w_full())))
                    .children(engine.map(|engine| div().text_size(px(11.0)).text_color(theme::text_muted()).child(format!("engine {engine}")))),
            )
            .child(div().flex_1().min_h_0().flex().flex_col().child(body))
            .children((!notes.is_empty()).then(|| {
                div().flex_shrink_0().px(px(12.0)).py(px(4.0)).flex().flex_col().gap(px(2.0)).children(notes.into_iter().take(4).enumerate().map(|(index, message)| {
                    div().debug_selector(move || format!("transfer-note-{index}")).text_size(px(11.5)).text_color(theme::warning()).child(message)
                }))
            }))
            .into_any_element()
    }

    /// One row of the columns of the selected table.
    fn column_row(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let tab_id = self.tab_id;
        let Some(job) = self.jobs.get(self.selected) else { return div().into_any_element() };
        let source = job.columns.ready().and_then(|columns| columns.get(index)).cloned();
        let row = div().id(("transfer-column-row", index)).h(px(COLUMN_ROW_HEIGHT)).px(px(12.0)).flex().items_center().gap(px(8.0));
        let selected = self.selected;
        if job.action == Action::Create {
            let Some(new) = job.new_columns.get(index) else { return row.into_any_element() };
            let include = new.include;
            let source_name = source.as_ref().map(|column| column.name.clone()).unwrap_or_default();
            let source_type = source.map(|column| column.data_type).unwrap_or_default();
            let types: Vec<&'static str> = self.dialect().map(|dialect| dialect.common_types().to_vec()).unwrap_or_default();
            let Some((name_input, type_input)) = self.ensure_new_inputs(index, window, cx) else { return row.into_any_element() };
            let type_menu_input = type_input.clone();
            let type_note = self.jobs[selected].type_notes.get(index).copied().flatten();
            return row
                .child(div().debug_selector(move || format!("transfer-include-{index}")).child(
                    Checkbox::new(gpui::ElementId::Name(format!("transfer-column-include-{tab_id}-{selected}-{index}").into())).checked(include).on_click(cx.listener(move |this, checked: &bool, _, cx| this.set_include(index, *checked, cx))),
                ))
                .child(div().w(px(140.0)).flex_shrink_0().truncate().text_size(px(12.0)).child(source_name))
                .child(div().w(px(110.0)).flex_shrink_0().truncate().text_size(px(11.0)).text_color(theme::type_text()).child(source_type))
                .child(Icon::new(IconName::ArrowRight).size(px(12.0)).text_color(theme::text_faint()))
                .child(div().debug_selector(move || format!("transfer-new-column-{index}")).w(px(170.0)).child(Input::new(&name_input).small().disabled(!include)))
                .child(
                    div().flex().items_center().gap(px(2.0)).child(div().debug_selector(move || format!("transfer-new-type-{index}")).w(px(150.0)).child(Input::new(&type_input).small().disabled(!include))).child(
                        Button::new(gpui::ElementId::Name(format!("transfer-types-{tab_id}-{selected}-{index}").into()))
                            .icon(IconName::ChevronDown)
                            .ghost()
                            .compact()
                            .disabled(!include)
                            .tooltip("Common types")
                            .dropdown_menu(move |menu, _, _| {
                                types.iter().fold(menu, |menu, &data_type| {
                                    let type_input = type_menu_input.clone();
                                    menu.item(PopupMenuItem::new(data_type).on_click(move |_, window, cx| type_input.update(cx, |state, cx| state.set_value(data_type, window, cx))))
                                })
                            }),
                    ),
                )
                .child(div().debug_selector(move || format!("transfer-type-note-{index}")).flex_1().min_w_0().truncate().text_size(px(11.0)).text_color(theme::warning()).children(type_note))
                .into_any_element();
        }
        let Some(select) = self.ensure_map_select(index, window, cx) else { return row.into_any_element() };
        let job = &self.jobs[selected];
        let source_name = source.as_ref().map(|column| column.name.clone()).unwrap_or_default();
        let source_type = source.map(|column| column.data_type).unwrap_or_default();
        let target_type = job
            .assignments
            .get(index)
            .copied()
            .flatten()
            .and_then(|target| job.target.ready().and_then(|target_desc| target_desc.columns.get(target)).map(|column| column.data_type.clone()))
            .unwrap_or_default();
        row.child(div().w(px(150.0)).flex_shrink_0().truncate().text_size(px(12.0)).child(source_name))
            .child(div().w(px(110.0)).flex_shrink_0().truncate().text_size(px(11.0)).text_color(theme::type_text()).child(source_type))
            .child(Icon::new(IconName::ArrowRight).size(px(12.0)).text_color(theme::text_faint()))
            .child(div().debug_selector(move || format!("transfer-map-{index}")).w(px(240.0)).child(Select::new(&select).small().w_full().menu_max_h(px(260.0))))
            .child(div().debug_selector(move || format!("transfer-map-type-{index}")).w(px(130.0)).flex_shrink_0().truncate().text_size(px(11.0)).text_color(theme::type_text()).child(target_type))
            .into_any_element()
    }

    // ---- 4. Run ----------------------------------------------------------------------------------------------

    fn render_run(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let body = if let Some(run) = &self.run {
            self.render_progress(run, cx).into_any_element()
        } else if let Some(report) = &self.report {
            self.render_summary(report, cx).into_any_element()
        } else {
            self.render_settings(cx).into_any_element()
        };
        div().debug_selector(|| "transfer-run-panel".into()).flex_1().min_h_0().flex().flex_col().child(body)
    }

    fn render_settings(&self, cx: &gpui::App) -> impl IntoElement {
        let files = self.target_kind == TargetKind::File;
        let active = self.jobs.iter().filter(|job| job.active()).count();
        let creating = self.tables_to_create();
        let problems: usize = self.jobs.iter().filter(|job| job.active()).map(|job| job.notes().len()).sum();
        let transactions = self.transactions();
        // Tables whose pages may not follow each other: a few lines, then how many more.
        let mut order_warnings: Vec<String> = self.jobs.iter().filter(|job| job.active()).filter_map(|job| job.query.order.warning(&job.label)).collect();
        if order_warnings.len() > 3 {
            let more = order_warnings.len() - 3;
            order_warnings.truncate(3);
            order_warnings.push(format!("\u{2026}and {more} more."));
        }
        div()
            .p(px(12.0))
            .flex()
            .flex_col()
            .gap(px(12.0))
            .child(div().debug_selector(|| "transfer-run-summary".into()).text_size(px(12.5)).child(if files {
                format!("{} will be written as {} files to {}.", plural(active, "table"), self.format.label(), self.folder.read(cx).value().trim())
            } else {
                format!("{} will be copied into {}{}.", plural(active, "table"), self.target.label(), if creating > 0 { format!(" ({creating} to create first)") } else { String::new() })
            }))
            .children((problems > 0 && !files).then(|| {
                div().debug_selector(|| "transfer-run-problems".into()).text_size(px(11.5)).text_color(theme::warning()).child(format!(
                    "{problems} possible problem{} found in the mapping. Rows the server refuses follow the error policy below.",
                    if problems == 1 { "" } else { "s" }
                ))
            }))
            .children((creating > 0 && !files).then(|| {
                div().text_size(px(12.0)).text_color(theme::text_muted()).child("The tables to create are shown as SQL for review, and nothing runs until you apply it.")
            }))
            .child(div().debug_selector(|| "transfer-snapshot-note".into()).text_size(px(11.5)).text_color(theme::text_muted()).child(
                "Each table is read as one snapshot where the database can (PostgreSQL, MySQL and MariaDB; SQLite in WAL mode), so it does not change between one page and the next. The snapshot holds a read lock on the table and keeps the server's old row versions while the table is copied, and is released as soon as the table is done.",
            ))
            .children(utc_query_note(self).map(|note| div().debug_selector(|| "transfer-run-query-utc".into()).text_size(px(11.5)).text_color(theme::text_muted()).child(note)))
            .children((!order_warnings.is_empty()).then(|| {
                div().flex().flex_col().gap(px(2.0)).children(order_warnings.into_iter().enumerate().map(|(index, text)| {
                    div().debug_selector(move || format!("transfer-order-warning-{index}")).text_size(px(11.5)).text_color(theme::warning()).child(text)
                }))
            }))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(12.0))
                    .when(!files, |this| {
                        this.child(field("Rows per batch", 130.0, div().debug_selector(|| "transfer-batch".into()).child(Input::new(&self.batch_size).small())))
                            .child(field("Transactions", 230.0, div().debug_selector(|| "transfer-transaction".into()).child(Select::new(&self.transaction_select).small().w_full().disabled(!transactions))))
                    })
                    .child(field("When something is refused", 260.0, div().debug_selector(|| "transfer-on-error".into()).child(Select::new(&self.on_error_select).small().w_full()))),
            )
            .when(!files, |this| {
                this.child(div().debug_selector(|| "transfer-transaction-note".into()).text_size(px(11.5)).text_color(theme::text_muted()).child(
                    self.no_transactions_note().unwrap_or_else(|| "Commit every batch keeps the batches already loaded if the transfer stops. One transaction per table keeps nothing of a table unless all of it loads.".to_owned()),
                ))
            })
            .child(div().text_size(px(11.5)).text_color(theme::text_muted()).child(if files {
                "A table that cannot be written is listed in the summary; with \"stop\" the tables after it are not written."
            } else {
                "The source is read on a session of its own and written on another, in pages and batches: memory stays at one of each, whatever the size of the tables."
            }))
    }

    fn render_progress(&self, run: &super::view::RunningTransfer, cx: &mut Context<Self>) -> impl IntoElement {
        let progress = &run.progress;
        let total = progress.jobs.len();
        let current = progress.current.load(std::sync::atomic::Ordering::Relaxed);
        let (mut read, mut written, mut skipped, mut bytes) = (0u64, 0u64, 0u64, 0u64);
        for job in &progress.jobs {
            let snapshot = job.snapshot();
            (read, written, skipped, bytes) = (read + snapshot.rows_read, written + snapshot.inserted, skipped + snapshot.skipped, bytes + snapshot.bytes);
        }
        let seconds = run.started.elapsed().as_secs_f64();
        let rate = if seconds > 0.0 { written as f64 / seconds } else { 0.0 };
        let fraction = if total > 0 { (current as f32 / total as f32).clamp(0.0, 1.0) } else { 0.0 };
        div()
            .flex_1()
            .min_h_0()
            .p(px(12.0))
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(div().debug_selector(|| "transfer-progress-title".into()).text_size(px(13.0)).child(if run.cancelling {
                "Cancelling\u{2026}".to_owned()
            } else {
                format!("Transferring table {} of {total}: {}", (current + 1).min(total), progress_label(&self.run_labels, current))
            }))
            .child(div().debug_selector(|| "transfer-progress-bar".into()).w_full().child(Progress::new(("transfer-bar", self.tab_id)).value(fraction * 100.0)))
            .child(
                div()
                    .debug_selector(|| "transfer-progress".into())
                    .flex()
                    .flex_col()
                    .gap(px(3.0))
                    .text_size(px(12.0))
                    .text_color(theme::text_muted())
                    .child(format!("{} rows read, {} written, {} skipped", grouped(read), grouped(written), grouped(skipped)))
                    .child(format!("{} read \u{00B7} {} rows/s \u{00B7} {:.0} s", human_bytes(bytes), grouped(rate as u64), seconds)),
            )
            .child(div().flex_1().min_h_0().border_1().border_color(theme::border()).rounded(px(4.0)).child(
                uniform_list(
                    "transfer-running-rows",
                    self.run_labels.len(),
                    cx.processor(|this, range: Range<usize>, _window, _cx| range.filter_map(|index| this.running_row(index)).collect::<Vec<_>>()),
                )
                .size_full(),
            ))
    }

    /// One table of the transfer in flight: waiting, running with its counters, or how it ended.
    fn running_row(&self, index: usize) -> Option<AnyElement> {
        let run = self.run.as_ref()?;
        let label = self.run_labels.get(index)?.clone();
        let current = run.progress.current.load(std::sync::atomic::Ordering::Relaxed);
        let finished = run.progress.finished_lines();
        let snapshot = run.progress.jobs.get(index)?.snapshot();
        let (text, color) = match finished.get(index).and_then(Option::as_ref) {
            Some(line) => outcome_line(line),
            None if index == current => (format!("{} rows read, {} written", grouped(snapshot.rows_read), grouped(snapshot.inserted)), theme::accent()),
            None => ("waiting".to_owned(), theme::text_faint()),
        };
        Some(
            div()
                .id(("transfer-running-row", index))
                .debug_selector(move || format!("transfer-running-{index}"))
                .w_full()
                .h(px(RESULT_ROW_HEIGHT))
                .px(px(8.0))
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(div().w(px(220.0)).flex_shrink_0().truncate().text_size(px(12.0)).child(label))
                .child(div().flex_1().min_w_0().truncate().text_size(px(11.5)).text_color(color).child(text))
                .into_any_element(),
        )
    }

    fn render_summary(&self, report: &super::job::TransferReport, cx: &mut Context<Self>) -> impl IntoElement {
        let (mut read, mut written, mut skipped, mut failed) = (0u64, 0u64, 0u64, 0usize);
        for result in report.jobs.iter().flatten() {
            read += result.report.rows_read;
            written += result.report.inserted;
            skipped += result.report.errors_total;
            failed += usize::from(matches!(result.report.outcome, Outcome::Failed(_) | Outcome::Stopped(_)));
        }
        let ran = report.jobs.iter().flatten().count();
        let files = self.target_kind == TargetKind::File;
        let (headline, color) = match &report.outcome {
            TransferOutcome::Completed if failed == 0 && skipped == 0 => (format!("Transferred {}: {} rows {}", plural(ran, "table"), grouped(written), if files { "written to files" } else { "copied" }), theme::success()),
            TransferOutcome::Completed => (format!("Transferred {}, {failed} failed, {} rows skipped", plural(ran - failed, "table"), grouped(skipped)), theme::warning()),
            TransferOutcome::Stopped(reason) => (format!("Stopped: {reason}"), theme::danger()),
            TransferOutcome::Cancelled => ("Cancelled".to_owned(), theme::warning()),
            TransferOutcome::Failed(message) => (format!("Failed: {message}"), theme::danger()),
        };
        let not_run = report.jobs.iter().filter(|job| job.is_none()).count();
        let selected = report.jobs.get(self.result_selected).and_then(Option::as_ref);
        let detail = selected.map(|result| {
            let message = match &result.report.outcome {
                Outcome::Failed(message) | Outcome::Stopped(message) => Some(message.clone()),
                _ => None,
            };
            (message, result.file.clone(), result.note.clone(), result.snapshot.clone())
        });
        // Why some table was not read as one snapshot (each reason once).
        let mut reasons: Vec<&str> = Vec::new();
        for result in report.jobs.iter().flatten() {
            if let SnapshotState::Unavailable(reason) = &result.snapshot {
                if !reasons.contains(reason) {
                    reasons.push(reason);
                }
            }
        }
        // Every table's rows left out, in one list.
        let listed: usize = report.jobs.iter().flatten().map(|result| result.report.errors.len()).sum();
        let heading = self.left_out_heading().unwrap_or_default();
        div()
            .debug_selector(|| "transfer-summary".into())
            .flex_1()
            .min_h_0()
            .p(px(12.0))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(div().debug_selector(|| "transfer-headline".into()).text_size(px(13.0)).text_color(color).child(headline))
            .child(div().debug_selector(|| "transfer-stats".into()).text_size(px(12.0)).text_color(theme::text_muted()).child(format!(
                "{} rows read \u{00B7} {} written \u{00B7} {} skipped \u{00B7} {} not started \u{00B7} {:.1} s",
                grouped(read),
                grouped(written),
                grouped(skipped),
                not_run,
                report.elapsed.as_secs_f64()
            )))
            .child(div().flex_1().min_h_0().border_1().border_color(theme::border()).rounded(px(4.0)).child(
                uniform_list(
                    "transfer-result-rows",
                    report.jobs.len(),
                    cx.processor(|this, range: Range<usize>, _window, cx| range.filter_map(|index| this.result_row(index, cx)).collect::<Vec<_>>()),
                )
                .size_full()
                .track_scroll(&self.results_scroll),
            ))
            .child(
                div()
                    .debug_selector(|| "transfer-snapshot".into())
                    .text_size(px(11.5))
                    .text_color(if reasons.is_empty() { theme::text_muted() } else { theme::warning() })
                    .child(if reasons.is_empty() {
                        "Each table was read as one snapshot: as it was when its copy began.".to_owned()
                    } else {
                        format!("Not read as one snapshot ({}): rows changed while a table was copied may be missed or read twice.", reasons.join("; "))
                    }),
            )
            .children(detail.map(|(message, file, note, snapshot)| {
                div()
                    .flex_shrink_0()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .children(message.map(|message| div().debug_selector(|| "transfer-failure".into()).text_size(px(12.0)).text_color(theme::danger()).child(message)))
                    .children(file.map(|(path, size)| div().debug_selector(|| "transfer-file".into()).text_size(px(11.5)).text_color(theme::text_muted()).child(format!("{} ({})", path.display(), human_bytes(size)))))
                    .child(div().debug_selector(|| "transfer-snapshot-table".into()).text_size(px(11.5)).text_color(theme::text_muted()).child(match snapshot {
                        SnapshotState::Held => "This table was read as one snapshot.".to_owned(),
                        SnapshotState::Unavailable(reason) => format!("This table was not read as one snapshot: {reason}."),
                    }))
                    .children(note.map(|note| div().debug_selector(|| "transfer-note".into()).text_size(px(11.5)).text_color(theme::warning()).child(note)))
            }))
            .children((listed > 0).then(|| {
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(div().debug_selector(|| "transfer-errors-heading".into()).text_size(px(11.5)).text_color(theme::text_muted()).child(heading))
                    .child(div().debug_selector(|| "transfer-errors".into()).flex_1().min_h_0().border_1().border_color(theme::border()).rounded(px(4.0)).child(
                        uniform_list(
                            "transfer-error-rows",
                            listed,
                            cx.processor(move |this, range: Range<usize>, _window, _cx| {
                                range
                                    .filter_map(|index| {
                                        let line = this.error_line(index)?;
                                        Some(
                                            div()
                                                .id(("transfer-error", index))
                                                .debug_selector(move || format!("transfer-error-{index}"))
                                                .w_full()
                                                .h(px(ERROR_ROW_HEIGHT))
                                                .px(px(8.0))
                                                .flex()
                                                .items_center()
                                                .truncate()
                                                .text_size(px(11.5))
                                                .child(line)
                                                .into_any_element(),
                                        )
                                    })
                                    .collect::<Vec<_>>()
                            }),
                        )
                        .size_full()
                        .track_scroll(&self.errors_scroll),
                    ))
            }))
    }

    /// The title of the list of rows left out of the transfer (`None` before it ran, or when no row was left out).
    pub(super) fn left_out_heading(&self) -> Option<String> {
        let report = self.report.as_ref()?;
        let left_out: Vec<LeftOut> = report
            .jobs
            .iter()
            .enumerate()
            .filter_map(|(index, job)| {
                let result = job.as_ref()?;
                (result.report.errors_total > 0).then(|| LeftOut { table: self.run_labels.get(index).map_or_else(String::new, ToString::to_string), skipped: result.report.errors_total, listed: result.report.errors.len() })
            })
            .collect();
        (!left_out.is_empty()).then(|| left_out_heading(&left_out, report.jobs.len() > 1))
    }

    /// The `n`th row left out of the whole transfer, with the table it belongs to when there are several.
    pub(super) fn error_line(&self, mut n: usize) -> Option<String> {
        let report = self.report.as_ref()?;
        for (index, job) in report.jobs.iter().enumerate() {
            let Some(result) = job else { continue };
            let errors = &result.report.errors;
            if n < errors.len() {
                let text = errors[n].to_string();
                return Some(if report.jobs.len() > 1 { format!("{} \u{00B7} {text}", self.run_labels.get(index)?) } else { text });
            }
            n -= errors.len();
        }
        None
    }

    /// One table of the summary: how it ended, in numbers.
    fn result_row(&self, index: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        let report = self.report.as_ref()?;
        let label = self.run_labels.get(index)?.clone();
        let (text, color) = match report.jobs.get(index)? {
            Some(result) => outcome_line(&FinishedLine { outcome: result.report.outcome.clone(), rows_read: result.report.rows_read, written: result.report.inserted, skipped: result.report.errors_total }),
            None => ("not started".to_owned(), theme::text_faint()),
        };
        let selected = index == self.result_selected;
        Some(
            div()
                .id(("transfer-result-row", index))
                .debug_selector(move || format!("transfer-result-{index}"))
                .w_full()
                .h(px(RESULT_ROW_HEIGHT))
                .px(px(8.0))
                .flex()
                .items_center()
                .gap(px(8.0))
                .when(selected, |this| this.bg(theme::selected()))
                .on_click(cx.listener(move |this, _, _, cx| this.select_result(index, cx)))
                .child(div().w(px(220.0)).flex_shrink_0().truncate().text_size(px(12.0)).child(label))
                .child(div().flex_1().min_w_0().truncate().text_size(px(11.5)).text_color(color).child(text))
                .into_any_element(),
        )
    }

    // ---- the buttons -----------------------------------------------------------------------------------------

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tab_id = self.tab_id;
        let button = |id: &'static str, label: &'static str| Button::new((id, tab_id)).debug_selector(move || id.into()).label(label).compact();
        let mut buttons: Vec<AnyElement> = Vec::new();
        if self.run.is_some() {
            let cancelling = self.run.as_ref().is_some_and(|run| run.cancelling);
            buttons.push(button("transfer-cancel", "Cancel transfer").danger().disabled(cancelling).on_click(cx.listener(|this, _, _, cx| this.cancel(cx))).into_any_element());
        } else if self.report.is_some() {
            buttons.push(button("transfer-again", "New transfer").ghost().on_click(cx.listener(|this, _, _, cx| this.again(cx))).into_any_element());
            buttons.push(button("transfer-close", "Close").primary().on_click(cx.listener(|this, _, window, cx| this.close_tab(window, cx))).into_any_element());
        } else {
            if self.step != Step::Source {
                buttons.push(button("transfer-back", "Back").ghost().on_click(cx.listener(|this, _, _, cx| this.back(cx))).into_any_element());
            }
            match self.step {
                Step::Source => buttons.push(button("transfer-next", "Next").primary().disabled(!self.can_leave_source(cx)).on_click(cx.listener(|this, _, window, cx| this.next(window, cx))).into_any_element()),
                Step::Target => buttons.push(button("transfer-next", "Next").primary().on_click(cx.listener(|this, _, window, cx| this.next(window, cx))).into_any_element()),
                Step::Mapping => buttons.push(button("transfer-next", "Next").primary().disabled(!self.can_leave_mapping()).on_click(cx.listener(|this, _, window, cx| this.next(window, cx))).into_any_element()),
                Step::Run => {
                    let label = match (self.target_kind, self.tables_to_create()) {
                        (TargetKind::File, _) => "Export",
                        (TargetKind::Table, 0) => "Transfer",
                        (TargetKind::Table, _) => "Create tables\u{2026}",
                    };
                    buttons.push(button("transfer-start", label).primary().on_click(cx.listener(|this, _, window, cx| this.begin_run(window, cx))).into_any_element())
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
            .child(div().debug_selector(|| "transfer-message".into()).flex_1().min_w_0().truncate().text_size(px(11.5)).text_color(theme::danger()).children(self.message.clone()))
            .children(buttons)
    }
}

/// A table that left rows out of a transfer: how many, and how many of them are listed.
struct LeftOut {
    table: String,
    skipped: u64,
    listed: usize,
}

/// The title of the list of rows left out: which tables (when the transfer has several) and, where a table has more than the
/// first [`ERROR_LOG_CAP`] rows, that only those are listed.
fn left_out_heading(tables: &[LeftOut], multi: bool) -> String {
    const NAMED: usize = 3;
    let mut heading = "Rows left out".to_owned();
    if multi {
        let names: Vec<&str> = tables.iter().take(NAMED).map(|table| table.table.as_str()).collect();
        let more = tables.len().saturating_sub(NAMED);
        heading.push_str(&format!(" of {} ({}{})", plural(tables.len(), "table"), names.join(", "), if more > 0 { format!(" and {more} more") } else { String::new() }));
    }
    if tables.iter().any(|table| table.skipped > table.listed as u64) {
        let total: u64 = tables.iter().map(|table| table.skipped).sum();
        heading.push_str(&format!(" \u{2014} {}, {} in all", if multi { format!("the first {ERROR_LOG_CAP} of each table are listed") } else { format!("the first {ERROR_LOG_CAP} are listed") }, grouped(total)));
    }
    heading.push(':');
    heading
}

fn progress_label(labels: &[SharedString], index: usize) -> SharedString {
    labels.get(index).cloned().unwrap_or_default()
}

/// How a table ended, in words and the color of them.
fn outcome_line(line: &FinishedLine) -> (String, gpui::Rgba) {
    match &line.outcome {
        Outcome::Completed if line.skipped == 0 => (format!("{} rows read, {} written", grouped(line.rows_read), grouped(line.written)), theme::success()),
        Outcome::Completed => (format!("{} rows read, {} written, {} skipped", grouped(line.rows_read), grouped(line.written), grouped(line.skipped)), theme::warning()),
        Outcome::Stopped(reason) => (format!("stopped: {reason}"), theme::danger()),
        Outcome::Cancelled => (format!("cancelled after {} rows written", grouped(line.written)), theme::warning()),
        Outcome::Failed(reason) => (format!("failed: {reason}"), theme::danger()),
    }
}

impl Render for TransferView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.poll_tables(cx);
        if self.step == Step::Mapping && self.target_kind == TargetKind::Table {
            self.ensure_widgets(window, cx);
        }
        let panel = match self.step {
            Step::Source => self.render_source(cx).into_any_element(),
            Step::Target => self.render_target(cx).into_any_element(),
            Step::Mapping => self.render_mapping(cx).into_any_element(),
            Step::Run => self.render_run(cx).into_any_element(),
        };
        div()
            .id(("transfer-view", self.tab_id))
            .debug_selector(|| "transfer-view".into())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn left(table: &str, skipped: u64, listed: usize) -> LeftOut {
        LeftOut { table: table.to_owned(), skipped, listed }
    }

    #[test]
    fn the_rows_left_out_are_titled_with_their_tables_and_what_is_listed_of_them() {
        // One table: no names, and the cap only when it cut the list.
        assert_eq!(left_out_heading(&[left("orders", 3, 3)], false), "Rows left out:");
        assert_eq!(left_out_heading(&[left("orders", 1_500, 200)], false), "Rows left out \u{2014} the first 200 are listed, 1,500 in all:");
        // Several tables: the ones that left rows out, singular or plural, a few named and the rest counted.
        assert_eq!(left_out_heading(&[left("orders", 2, 2)], true), "Rows left out of 1 table (orders):");
        assert_eq!(left_out_heading(&[left("orders", 2, 2), left("items", 1, 1)], true), "Rows left out of 2 tables (orders, items):");
        assert_eq!(
            left_out_heading(&[left("a", 1, 1), left("b", 1, 1), left("c", 1, 1), left("d", 1, 1), left("e", 1, 1)], true),
            "Rows left out of 5 tables (a, b, c and 2 more):"
        );
        assert_eq!(
            left_out_heading(&[left("orders", 900, 200), left("items", 1, 1)], true),
            "Rows left out of 2 tables (orders, items) \u{2014} the first 200 of each table are listed, 901 in all:"
        );
    }
}
