use super::super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::app) enum ContextExportFormat {
    Csv,
    CsvWithHeaders,
    Json,
    Markdown,
    Sql,
    Text,
    Xlsx,
}

impl ContextExportFormat {
    pub(in crate::app) const ALL: &'static [(Self, &'static str)] = &[
        (Self::Csv, "CSV"),
        (Self::CsvWithHeaders, "CSV with Headers"),
        (Self::Xlsx, "XLSX"),
        (Self::Json, "JSON"),
        (Self::Markdown, "Markdown"),
        (Self::Sql, "SQL INSERT"),
        (Self::Text, "Text with Headers"),
    ];
}

/// Writes `data` to `path`; the rows written and, for JSON, a note when
/// repeated column names had to be made unique. `defaults` are the "export
/// defaults" settings: CSV uses their delimiter, header choice and NULL text.
pub(in crate::app) fn export_context_data(
    path: &std::path::Path,
    data: &super::copy::CopyData,
    format: ContextExportFormat,
    defaults: &crate::settings::ExportDefaults,
) -> Result<(usize, Option<String>), String> {
    // The delimited formats are written row by row to the file.
    let dsv = match format {
        ContextExportFormat::Csv | ContextExportFormat::CsvWithHeaders => {
            Some(super::copy::DsvOptions {
                column_separator: defaults.delimiter.clone(),
                null_text: defaults.null_text.clone(),
                include_headers: format == ContextExportFormat::CsvWithHeaders || defaults.header,
                ..Default::default()
            })
        }
        ContextExportFormat::Text => Some(super::copy::DsvOptions::single('\t', true)),
        _ => None,
    };
    if let Some(options) = dsv {
        let file = std::fs::File::create(path).map_err(|error| error.to_string())?;
        let mut writer = std::io::BufWriter::new(file);
        let written = data
            .write_dsv(&mut writer, &options)
            .and_then(|()| std::io::Write::flush(&mut writer).map_err(|error| error.to_string()));
        if let Err(error) = written {
            drop(writer);
            let _ = std::fs::remove_file(path);
            return Err(error);
        }
        return Ok((data.rows.len(), None));
    }
    if format == ContextExportFormat::Xlsx {
        let mut writer = TableExporter::create(
            path,
            ExportFormat::Xlsx,
            ExportDialect::Postgres,
            data.table.clone(),
            data.columns.clone(),
        )?;
        for row in &data.rows {
            let values = row
                .iter()
                .map(|cell| cell.plain_text().to_string())
                .collect::<Vec<_>>();
            writer.write_rows(&[values])?;
        }
        return Ok((writer.finish()?, None));
    }
    let copy_format = match format {
        ContextExportFormat::Json => super::copy::CopyFormat::Json,
        ContextExportFormat::Markdown => super::copy::CopyFormat::Markdown,
        ContextExportFormat::Sql => super::copy::CopyFormat::SqlInsert,
        ContextExportFormat::Csv
        | ContextExportFormat::CsvWithHeaders
        | ContextExportFormat::Text
        | ContextExportFormat::Xlsx => unreachable!(),
    };
    // A file is not the clipboard: no copy limit.
    std::fs::write(path, data.format_up_to(copy_format, usize::MAX)?).map_err(|error| error.to_string())?;
    let notice = (format == ContextExportFormat::Json)
        .then(|| super::copy::renamed_columns_notice(&data.json_columns().1))
        .flatten();
    Ok((data.rows.len(), notice))
}

/// What the background job needs: everything is owned, nothing reaches back into the app.
struct ExportJob {
    path: std::path::PathBuf,
    data: super::copy::CopyData,
    format: ContextExportFormat,
    defaults: crate::settings::ExportDefaults,
}

impl AppRoot {
    /// Collects the cells on the UI thread (one pass over what is on the page, no formatting) and
    /// hands them to the background executor, which creates the folder, formats and writes the file
    /// (`export_context_data`): a large page never blocks the window. A notification says the export
    /// started, the dialog says how it ended.
    pub(in crate::app) fn export_context_selection(
        &mut self,
        tab_id: u64,
        format: ContextExportFormat,
        entire_page: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let job = (|| {
            let tab = self.workspace.tab(tab_id).ok_or("No active result tab")?;
            if format == ContextExportFormat::Sql && tab.query_mode.is_mongo_find()
            {
                return Err("SQL export is unavailable for MongoDB".into());
            }
            let row_end = self
                .grid(tab_id)
                .row_count()
                .checked_sub(1)
                .ok_or("No rows to export")?;
            let col_end = self
                .display_column_count(tab_id)
                .checked_sub(1)
                .ok_or("No columns to export")?;
            let range = if entire_page {
                crate::grid::CellRange {
                    start: CellPosition { row: 0, column: 0 },
                    end: CellPosition {
                        row: row_end,
                        column: col_end,
                    },
                }
            } else {
                self.view(tab_id)
                    .grid_ui
                    .selection
                    .borrow()
                    .range
                    .ok_or("Select cells to export")?
            };
            let data = self.collect_copy_context_range_up_to(tab_id, range, usize::MAX)?;
            if format == ContextExportFormat::Sql {
                self.validate_sql_copy_context(
                    tab_id,
                    data.columns.first().ok_or("No SQL column selected")?,
                )?;
            }
            let ext = match format {
                ContextExportFormat::Csv | ContextExportFormat::CsvWithHeaders => "csv",
                ContextExportFormat::Json => "json",
                ContextExportFormat::Markdown => "md",
                ContextExportFormat::Sql => "sql",
                ContextExportFormat::Text => "txt",
                ContextExportFormat::Xlsx => "xlsx",
            };
            let safe_table = tab
                .collection
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect::<String>();
            let folder = Some(self.settings.results.export.folder.trim())
                .filter(|folder| !folder.is_empty())
                .map(std::path::PathBuf::from)
                .or_else(crate::platform::exports_dir);
            let path = folder
                .ok_or("Downloads directory unavailable")?
                .join(format!(
                    "{}-{}.{}",
                    if safe_table.is_empty() {
                        "query"
                    } else {
                        safe_table.as_str()
                    },
                    uuid::Uuid::new_v4(),
                    ext
                ));
            Ok::<_, String>(ExportJob { path, data, format, defaults: self.settings.results.export.clone() })
        })();
        let job = match job {
            Ok(job) => job,
            Err(error) => {
                self.export_finished(tab_id, Err(error), window, cx);
                return;
            }
        };
        window.push_notification(format!("Exporting {} rows\u{2026}", job.data.rows.len()), cx);
        let task = cx.background_executor().spawn(async move {
            let ExportJob { path, data, format, defaults } = job;
            let written = path
                .parent()
                .ok_or_else(|| "Export directory unavailable".to_string())
                .and_then(|folder| std::fs::create_dir_all(folder).map_err(|error| error.to_string()))
                .and_then(|()| export_context_data(&path, &data, format, &defaults));
            written.map(|(rows, notice)| (rows, notice, path))
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| this.export_finished(tab_id, result, window, cx));
        })
        .detach();
    }

    /// How an export ended: the dialog, and the error log for a failure.
    fn export_finished(
        &mut self,
        tab_id: u64,
        result: Result<(usize, Option<String>, std::path::PathBuf), String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok((rows, notice, path)) => window.open_alert_dialog(cx, move |dialog, _, _| {
                dialog.title("Export complete").child(format!(
                    "Exported {rows} rows to {}{}",
                    path.display(),
                    notice
                        .as_ref()
                        .map(|notice| format!("\n{notice}"))
                        .unwrap_or_default()
                ))
            }),
            Err(error) => {
                self.report_error(Area::Export, None, &error);
                window.open_alert_dialog(cx, move |dialog, _, _| {
                    dialog.title("Export failed").child(error.clone())
                })
            }
        }
        self.notify_tab(tab_id, cx);
    }
}

#[cfg(test)]
mod export_selection_tests {
    use super::*;

    #[test]
    fn export_formats_write_only_supplied_rows_into_temp_files() {
        let data = super::super::copy::CopyData {
            columns: vec!["id".into()],
            rows: vec![vec![super::super::copy::CopyCell::new("7", "int64", false)]],
            table: "orders".into(),
            schema: Some("sales".into()),
            key_column: Some("id".into()),
            keys: vec![super::super::copy::CopyCell::new("7", "int64", false)],
            target: super::super::copy::SqlTarget::POSTGRES,
        };
        let path = std::env::temp_dir().join(format!("tablex-export-{}.csv", uuid::Uuid::new_v4()));
        assert_eq!(
            export_context_data(
                &path,
                &data,
                ContextExportFormat::CsvWithHeaders,
                &crate::settings::ExportDefaults::default()
            )
            .unwrap()
            .0,
            1
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "id\n7");
        std::fs::remove_file(path).unwrap();
        let path =
            std::env::temp_dir().join(format!("tablex-export-{}.xlsx", uuid::Uuid::new_v4()));
        assert_eq!(
            export_context_data(
                &path,
                &data,
                ContextExportFormat::Xlsx,
                &crate::settings::ExportDefaults::default()
            )
            .unwrap()
            .0,
            1
        );
        assert!(std::fs::metadata(&path).unwrap().len() > 0);
        std::fs::remove_file(path).unwrap();
    }
}
