use serde::Serialize;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Csv,
    Json,
    Xlsx,
    SqlInsert,
}

impl ExportFormat {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "csv" => Ok(Self::Csv),
            "json" => Ok(Self::Json),
            "xlsx" => Ok(Self::Xlsx),
            "sql_insert" => Ok(Self::SqlInsert),
            other => Err(format!("unsupported export format: {other}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportDialect {
    Mongo,
    Postgres,
}

#[derive(Debug, Serialize)]
pub struct ExportResult {
    pub ok: bool,
    pub rows_exported: usize,
    pub path: String,
}

/// One cell of a row given to [`TableExporter::write_cells`]: typed, so a NULL is not the word `null`, and a text that
/// reads as a number stays a text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportCell<'a> {
    Null,
    Text(&'a str),
    /// The digits of a number (an XLSX cell holds it as a number).
    Number(&'a str),
    Bool(bool),
}

pub struct TableExporter {
    target: ExportTarget,
    format: ExportFormat,
    dialect: ExportDialect,
    table_name: String,
    columns: Vec<String>,
    rows_written: usize,
}

enum ExportTarget {
    Text(BufWriter<File>),
    Xlsx {
        workbook: Box<rust_xlsxwriter::Workbook>,
        path: PathBuf,
    },
}

impl TableExporter {
    pub fn create(
        path: impl AsRef<Path>,
        format: ExportFormat,
        dialect: ExportDialect,
        table_name: String,
        columns: Vec<String>,
    ) -> Result<Self, String> {
        if format == ExportFormat::SqlInsert && dialect == ExportDialect::Mongo {
            return Err("SQL INSERT export is only available for SQL databases".to_string());
        }
        let target = if format == ExportFormat::Xlsx {
            ExportTarget::Xlsx {
                workbook: Box::new(rust_xlsxwriter::Workbook::new()),
                path: path.as_ref().to_path_buf(),
            }
        } else {
            ExportTarget::Text(BufWriter::new(
                File::create(path.as_ref()).map_err(|error| error.to_string())?,
            ))
        };
        let mut exporter = Self {
            target,
            format,
            dialect,
            table_name,
            columns,
            rows_written: 0,
        };
        exporter.write_header()?;
        Ok(exporter)
    }

    pub fn write_rows(&mut self, rows: &[Vec<String>]) -> Result<(), String> {
        for row in rows {
            match self.format {
                ExportFormat::Csv => self.write_csv_row(row)?,
                ExportFormat::Json => self.write_json_row(row)?,
                ExportFormat::SqlInsert => self.write_sql_row(row)?,
                ExportFormat::Xlsx => self.write_xlsx_row(row)?,
            }
            self.rows_written += 1;
        }
        Ok(())
    }

    /// Writes typed rows. An XLSX sheet gets numbers, booleans and blanks as such; the text formats get the words
    /// `null`, `true` and `false` for NULL and booleans, as [`Self::write_rows`] reads them.
    pub fn write_cells(&mut self, row: &[ExportCell<'_>]) -> Result<(), String> {
        if let (ExportFormat::Xlsx, ExportTarget::Xlsx { workbook, .. }) = (self.format, &mut self.target) {
            let worksheet = workbook
                .worksheet_from_index(0)
                .map_err(|error| error.to_string())?;
            let row_index = u32::try_from(self.rows_written + 1)
                .map_err(|_| "an XLSX sheet holds at most 1,048,576 rows".to_string())?;
            for (index, cell) in row.iter().enumerate() {
                let column = u16::try_from(index)
                    .map_err(|_| "an XLSX sheet holds at most 16,384 columns".to_string())?;
                let written = match cell {
                    ExportCell::Null => continue,
                    ExportCell::Text(text) => worksheet.write_string(row_index, column, *text),
                    ExportCell::Number(digits) => match digits.parse::<f64>() {
                        // A number Excel would round (more than 15 digits) is kept as the text it was read as.
                        Ok(number) if number.is_finite() && digits.trim_start_matches('-').replace('.', "").trim_start_matches('0').len() <= 15 => {
                            worksheet.write_number(row_index, column, number)
                        }
                        _ => worksheet.write_string(row_index, column, *digits),
                    },
                    ExportCell::Bool(value) => worksheet.write_boolean(row_index, column, *value),
                };
                written.map_err(|error| error.to_string())?;
            }
            self.rows_written += 1;
            return Ok(());
        }
        let row: Vec<String> = row
            .iter()
            .map(|cell| match cell {
                ExportCell::Null => "null".to_string(),
                ExportCell::Text(text) | ExportCell::Number(text) => (*text).to_string(),
                ExportCell::Bool(value) => value.to_string(),
            })
            .collect();
        self.write_rows(&[row])
    }

    #[allow(dead_code)]
    pub fn columns(&self) -> &[String] {
        &self.columns
    }

    pub fn finish(mut self) -> Result<usize, String> {
        match self.format {
            ExportFormat::Json => self
                .text_writer()?
                .write_all(b"\n]")
                .map_err(|error| error.to_string())?,
            ExportFormat::SqlInsert if self.rows_written > 0 => self
                .text_writer()?
                .write_all(b";\n")
                .map_err(|error| error.to_string())?,
            _ => {}
        }
        match &mut self.target {
            ExportTarget::Text(writer) => writer.flush().map_err(|error| error.to_string())?,
            ExportTarget::Xlsx { workbook, path } => {
                workbook.save(path).map_err(|error| error.to_string())?
            }
        }
        Ok(self.rows_written)
    }

    fn write_header(&mut self) -> Result<(), String> {
        match self.format {
            ExportFormat::Csv => {
                let line = self
                    .columns
                    .iter()
                    .map(|value| csv_cell(value))
                    .collect::<Vec<_>>()
                    .join(",");
                writeln!(self.text_writer()?, "{line}").map_err(|error| error.to_string())
            }
            ExportFormat::Json => self
                .text_writer()?
                .write_all(b"[")
                .map_err(|error| error.to_string()),
            ExportFormat::SqlInsert => Ok(()),
            ExportFormat::Xlsx => {
                if let ExportTarget::Xlsx { workbook, .. } = &mut self.target {
                    // One row at a time goes to a temporary file: the sheet is never held (rows are written in order).
                    let worksheet = workbook.add_worksheet_with_constant_memory();
                    for (index, column) in self.columns.iter().enumerate() {
                        worksheet
                            .write_string(0, index as u16, column)
                            .map_err(|error| error.to_string())?;
                    }
                }
                Ok(())
            }
        }
    }

    fn write_csv_row(&mut self, row: &[String]) -> Result<(), String> {
        let line = row
            .iter()
            .map(|value| csv_cell(value))
            .collect::<Vec<_>>()
            .join(",");
        writeln!(self.text_writer()?, "{line}").map_err(|error| error.to_string())
    }

    fn write_json_row(&mut self, row: &[String]) -> Result<(), String> {
        if self.rows_written > 0 {
            self.text_writer()?
                .write_all(b",")
                .map_err(|error| error.to_string())?;
        }
        self.text_writer()?
            .write_all(b"\n  {")
            .map_err(|error| error.to_string())?;
        let columns = self.columns.clone();
        for (index, column) in columns.iter().enumerate() {
            if index > 0 {
                self.text_writer()?
                    .write_all(b",")
                    .map_err(|error| error.to_string())?;
            }
            let value = row.get(index).map(String::as_str).unwrap_or("");
            write!(
                self.text_writer()?,
                "\n    {}: {}",
                json_string(column),
                json_value(value)
            )
            .map_err(|error| error.to_string())?;
        }
        self.text_writer()?
            .write_all(b"\n  }")
            .map_err(|error| error.to_string())
    }

    fn write_sql_row(&mut self, row: &[String]) -> Result<(), String> {
        if self.rows_written == 0 {
            let table_name = quote_identifier(&self.table_name, self.dialect);
            let column_list = self
                .columns
                .iter()
                .map(|column| quote_identifier(column, self.dialect))
                .collect::<Vec<_>>()
                .join(", ");
            writeln!(
                self.text_writer()?,
                "INSERT INTO {} ({}) VALUES",
                table_name,
                column_list
            )
            .map_err(|error| error.to_string())?;
        } else {
            self.text_writer()?
                .write_all(b",\n")
                .map_err(|error| error.to_string())?;
        }
        let values = self
            .columns
            .iter()
            .enumerate()
            .map(|(index, _)| sql_literal(row.get(index).map(String::as_str).unwrap_or("")))
            .collect::<Vec<_>>()
            .join(", ");
        write!(self.text_writer()?, "  ({values})").map_err(|error| error.to_string())
    }

    fn write_xlsx_row(&mut self, row: &[String]) -> Result<(), String> {
        if let ExportTarget::Xlsx { workbook, .. } = &mut self.target {
            let worksheet = workbook
                .worksheet_from_index(0)
                .map_err(|error| error.to_string())?;
            let row_index = (self.rows_written + 1) as u32;
            for (index, value) in row.iter().enumerate() {
                worksheet
                    .write_string(row_index, index as u16, value)
                    .map_err(|error| error.to_string())?;
            }
        }
        Ok(())
    }

    fn text_writer(&mut self) -> Result<&mut BufWriter<File>, String> {
        match &mut self.target {
            ExportTarget::Text(writer) => Ok(writer),
            ExportTarget::Xlsx { .. } => Err("text writer requested for XLSX export".to_string()),
        }
    }
}

pub fn csv_cell(value: &str) -> String {
    if value.contains(',') || value.contains('"') || value.contains('\n') || value.contains('\r') {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

pub fn json_value(value: &str) -> String {
    if value == "null" || value == "true" || value == "false" {
        return value.to_string();
    }
    if value.parse::<i64>().is_ok() && !has_ambiguous_leading_zero(value) {
        return value.to_string();
    }
    if value.parse::<f64>().is_ok() && !has_ambiguous_leading_zero(value) {
        return value.to_string();
    }
    if (value.starts_with('{') || value.starts_with('['))
        && serde_json::from_str::<serde_json::Value>(value).is_ok()
    {
        return value.to_string();
    }
    json_string(value)
}

pub fn sql_literal(value: &str) -> String {
    if value == "null" {
        return "NULL".to_string();
    }
    if value == "true" || value == "false" {
        return value.to_ascii_uppercase();
    }
    if value.parse::<i64>().is_ok() && !has_ambiguous_leading_zero(value) {
        return value.to_string();
    }
    if value.parse::<f64>().is_ok() && !has_ambiguous_leading_zero(value) {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "''"))
}

pub fn quote_identifier(value: &str, dialect: ExportDialect) -> String {
    match dialect {
        ExportDialect::Mongo => format!("`{}`", value.replace('`', "``")),
        ExportDialect::Postgres => format!("\"{}\"", value.replace('"', "\"\"")),
    }
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

fn has_ambiguous_leading_zero(value: &str) -> bool {
    value.len() > 1 && value.starts_with('0') && !value.starts_with("0.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_cell_escapes_quotes_commas_and_newlines() {
        assert_eq!(csv_cell("Ada, \"Countess\""), "\"Ada, \"\"Countess\"\"\"");
        assert_eq!(csv_cell("Line\nBreak"), "\"Line\nBreak\"");
    }

    #[test]
    fn sql_literal_escapes_strings_and_preserves_numbers() {
        assert_eq!(sql_literal("42"), "42");
        assert_eq!(sql_literal("O'Brien"), "'O''Brien'");
        assert_eq!(sql_literal("null"), "NULL");
    }

    #[test]
    fn quote_identifier_quotes_postgres_names() {
        assert_eq!(
            quote_identifier("User \"Name\"", ExportDialect::Postgres),
            "\"User \"\"Name\"\"\""
        );
    }

    #[test]
    fn xlsx_export_streams_typed_cells_without_holding_the_sheet() {
        let path = std::env::temp_dir().join(format!("engine-export-xlsx-{}.xlsx", std::process::id()));
        let mut exporter = TableExporter::create(
            &path,
            ExportFormat::Xlsx,
            ExportDialect::Postgres,
            "t".into(),
            vec!["id".into(), "name".into(), "ok".into()],
        )
        .expect("create exporter");
        for id in 0..2000 {
            let number = id.to_string();
            exporter
                .write_cells(&[ExportCell::Number(&number), ExportCell::Text("a, \"b\""), ExportCell::Bool(id % 2 == 0)])
                .expect("write row");
        }
        exporter.write_cells(&[ExportCell::Null, ExportCell::Number("12345678901234567890"), ExportCell::Null]).expect("write row");
        assert_eq!(exporter.finish().expect("finish export"), 2001);
        assert!(std::fs::metadata(&path).expect("the workbook").len() > 1000);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn csv_export_writes_headers_and_escaped_values() {
        let path = std::env::temp_dir().join(format!(
            "tableleaf-export-test-{}-{}.csv",
            std::process::id(),
            std::thread::current().name().unwrap_or("csv")
        ));
        let mut exporter = TableExporter::create(
            &path,
            ExportFormat::Csv,
            ExportDialect::Postgres,
            "users".into(),
            vec!["id".into(), "name".into()],
        )
        .expect("create exporter");
        exporter
            .write_rows(&[vec!["1".into(), "Ada, Jr.".into()]])
            .expect("write row");
        assert_eq!(exporter.finish().expect("finish export"), 1);

        let output = std::fs::read_to_string(&path).expect("read export");
        assert_eq!(output, "id,name\n1,\"Ada, Jr.\"\n");
        let _ = std::fs::remove_file(path);
    }
}
