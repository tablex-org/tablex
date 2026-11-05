use super::super::*;
use super::column_type::{base_type_head, value_kind, ValueKind};
use crate::sql_filter::{is_numeric_literal, sql_string_literal};
use crate::sql_table::{BinaryLiteral, SqlDialect};

pub(in crate::app) const COPY_MAX_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug)]
pub(in crate::app) struct DsvOptions {
    pub column_separator: String,
    pub row_separator: String,
    /// What a SQL NULL is written as. Empty (the default) leaves the
    /// field empty; a real empty string is then written quoted (`""`), and a
    /// real string equal to a non-empty NULL text is quoted too, so a NULL can
    /// never read back as a value.
    pub null_text: String,
    pub quote: char,
    pub include_headers: bool,
}

impl Default for DsvOptions {
    fn default() -> Self {
        Self {
            column_separator: ",".into(),
            row_separator: "\n".into(),
            null_text: String::new(),
            quote: '"',
            include_headers: false,
        }
    }
}

impl DsvOptions {
    /// The delimited copy formats (TSV, CSV, pipe): one separator character.
    fn single(separator: char, include_headers: bool) -> Self {
        Self {
            column_separator: separator.to_string(),
            include_headers,
            ..Self::default()
        }
    }

    pub(in crate::app) fn validate(&self) -> Result<(), String> {
        if self.column_separator.is_empty()
            || self.row_separator.is_empty()
            || self.column_separator.chars().count() > 8
            || self.row_separator.chars().count() > 8
            || self.null_text.chars().count() > 64
            || self.quote.is_control()
            || self.column_separator.contains(self.quote)
            || self.row_separator.contains(self.quote)
            || self.column_separator.contains(&self.row_separator)
            || self.row_separator.contains(&self.column_separator)
        {
            return Err("Invalid DSV separator or quote settings".into());
        }
        Ok(())
    }

    /// `value` as one field. Quoted (quotes doubled) when it is empty, equals
    /// the NULL text, holds a separator, the quote, or any line break
    /// whatever the row separator is.
    pub(in crate::app) fn field<'a>(&self, value: &'a str) -> std::borrow::Cow<'a, str> {
        let needs_quote = value.is_empty()
            || value.contains(&self.column_separator)
            || value.contains(&self.row_separator)
            || value.contains(self.quote)
            || value.contains(['\r', '\n'])
            || value == self.null_text;
        if !needs_quote {
            return std::borrow::Cow::Borrowed(value);
        }
        let quote = self.quote;
        let mut out = String::with_capacity(value.len() + 2);
        out.push(quote);
        for ch in value.chars() {
            if ch == quote {
                out.push(quote);
            }
            out.push(ch);
        }
        out.push(quote);
        std::borrow::Cow::Owned(out)
    }

    fn cell<'a>(&'a self, cell: &'a CopyCell) -> std::borrow::Cow<'a, str> {
        if cell.missing {
            std::borrow::Cow::Borrowed("")
        } else if cell.is_null() {
            std::borrow::Cow::Borrowed(&self.null_text)
        } else {
            self.field(cell.raw.as_str())
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(in crate::app) enum CopyFormat {
    Smart,
    Raw,
    Tsv,
    TsvWithHeaders,
    Csv,
    CsvWithHeaders,
    Pipe,
    Dsv,
    Json,
    JsonLines,
    OneRow,
    SqlIn,
    SqlInsert,
    SqlInsertNoPk,
    SqlUpdate,
    SqlSelect,
    SqlWhere,
    Markdown,
    Html,
    Xml,
    Pretty,
}

pub(super) fn copy_default_path() -> Option<std::path::PathBuf> {
    dirs::config_dir().map(|directory| directory.join("TableX/grid-copy-default.json"))
}

pub(in crate::app) fn read_copy_default(path: &std::path::Path) -> CopyFormat {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(CopyFormat::Smart)
}

pub(in crate::app) fn write_copy_default(
    path: &std::path::Path,
    format: CopyFormat,
) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or("Copy preference directory unavailable")?;
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let temporary = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4()));
    let value = serde_json::to_vec(&format).map_err(|error| error.to_string())?;
    std::fs::write(&temporary, value).map_err(|error| error.to_string())?;
    if let Err(error) = std::fs::rename(&temporary, path) {
        let _ = std::fs::remove_file(temporary);
        return Err(error.to_string());
    }
    Ok(())
}

impl CopyFormat {
    /// The grid's copy format behind a "default copy format" setting choice.
    pub(in crate::app) fn from_setting(choice: crate::settings::CopyDefault) -> Self {
        use crate::settings::CopyDefault as Choice;
        match choice {
            Choice::Smart => Self::Smart,
            Choice::Raw => Self::Raw,
            Choice::Tsv => Self::Tsv,
            Choice::TsvWithHeaders => Self::TsvWithHeaders,
            Choice::Csv => Self::Csv,
            Choice::CsvWithHeaders => Self::CsvWithHeaders,
            Choice::Json => Self::Json,
            Choice::Markdown => Self::Markdown,
        }
    }

    /// The setting choice for this format; `None` for formats the setting
    /// does not offer (the context menu can still set those as its default).
    pub(in crate::app) fn to_setting(self) -> Option<crate::settings::CopyDefault> {
        use crate::settings::CopyDefault as Choice;
        Some(match self {
            Self::Smart => Choice::Smart,
            Self::Raw => Choice::Raw,
            Self::Tsv => Choice::Tsv,
            Self::TsvWithHeaders => Choice::TsvWithHeaders,
            Self::Csv => Choice::Csv,
            Self::CsvWithHeaders => Choice::CsvWithHeaders,
            Self::Json => Choice::Json,
            Self::Markdown => Choice::Markdown,
            _ => return None,
        })
    }

    pub(in crate::app) const ALL: &'static [(Self, &'static str)] = &[
        (Self::Raw, "Raw"),
        (Self::Tsv, "TSV"),
        (Self::TsvWithHeaders, "TSV with Headers"),
        (Self::Csv, "CSV"),
        (Self::CsvWithHeaders, "CSV with Headers"),
        (Self::Pipe, "Pipe Separated"),
        (Self::Dsv, "DSV"),
        (Self::Json, "JSON"),
        (Self::JsonLines, "JSON Lines"),
        (Self::OneRow, "One Row"),
        (Self::SqlIn, "SQL IN"),
        (Self::SqlInsert, "SQL INSERT with PK"),
        (Self::SqlInsertNoPk, "SQL INSERT without PK"),
        (Self::SqlUpdate, "SQL UPDATE"),
        (Self::SqlSelect, "SQL SELECT"),
        (Self::SqlWhere, "SQL WHERE"),
        (Self::Markdown, "Markdown"),
        (Self::Html, "HTML"),
        (Self::Xml, "XML"),
        (Self::Pretty, "Pretty JSON"),
    ];
}

#[derive(Clone, Copy)]
pub(in crate::app) enum CopyScope {
    Selected,
    Cell,
    Rows,
    Page,
    ColumnNames,
}

/// How a cell is written in SQL text, decided once from its column's type.
#[derive(Clone, Debug, PartialEq, Eq)]
enum SqlForm {
    Null,
    Boolean(bool),
    /// Bare, exact digits (never through a float).
    Number,
    Text,
    /// Bytes, written as the engine's binary literal.
    Binary,
    /// The value cannot be written as SQL; the reason is shown to the user.
    Unwritable(&'static str),
}

#[derive(Clone)]
pub(in crate::app) struct CopyCell {
    raw: String,
    value: serde_json::Value,
    form: SqlForm,
    /// The document has no such field (or a staged `$unset` removes it): a blank cell, copied as
    /// an empty field in the delimited formats and left out of a JSON row. Not a NULL.
    missing: bool,
}

impl CopyCell {
    /// `data_type` is the column's resolved type (or, for a cell whose storage
    /// class differs from its column's, the type of that class).
    pub(in crate::app) fn new(raw: &str, data_type: &str, is_null: bool) -> Self {
        let precise_decimal = ["decimal", "numeric", "money"]
            .iter()
            .any(|kind| data_type.to_ascii_lowercase().contains(kind));
        Self {
            raw: raw.to_string(),
            value: if precise_decimal && !is_null {
                serde_json::Value::String(raw.to_string())
            } else {
                row_value_to_json(raw, data_type, is_null)
            },
            form: Self::sql_form(raw, data_type, is_null),
            missing: false,
        }
    }

    /// As [`Self::new`], for a value the engine cannot take back as SQL text.
    pub(in crate::app) fn unwritable(raw: &str, data_type: &str, reason: &'static str) -> Self {
        Self {
            form: SqlForm::Unwritable(reason),
            ..Self::new(raw, data_type, false)
        }
    }

    fn sql_form(raw: &str, data_type: &str, is_null: bool) -> SqlForm {
        if is_null {
            return SqlForm::Null;
        }
        let trimmed = raw.trim();
        match value_kind(data_type) {
            ValueKind::Boolean => match trimmed.to_ascii_lowercase().as_str() {
                "true" | "t" => SqlForm::Boolean(true),
                "false" | "f" => SqlForm::Boolean(false),
                _ if is_numeric_literal(trimmed) => SqlForm::Number,
                _ => SqlForm::Text,
            },
            ValueKind::Integer | ValueKind::Decimal | ValueKind::Float
                if is_numeric_literal(trimmed) =>
            {
                SqlForm::Number
            }
            ValueKind::Binary if super::editing::is_hex_bytes(trimmed) => SqlForm::Binary,
            // PostgreSQL shows bytea as `\x` + hex (`0x` + hex where it is read as bytes).
            _ if base_type_head(data_type) == "bytea"
                && trimmed
                    .strip_prefix("\\x")
                    .or_else(|| trimmed.strip_prefix("0x"))
                    .is_some_and(|hex| hex.len() % 2 == 0 && hex.bytes().all(|b| b.is_ascii_hexdigit())) =>
            {
                SqlForm::Binary
            }
            _ => SqlForm::Text,
        }
    }

    pub(in crate::app) fn is_null(&self) -> bool {
        self.form == SqlForm::Null
    }

    /// The text snapshots and the grid show: `NULL` for a SQL NULL.
    pub(in crate::app) fn display(&self) -> &str {
        if self.is_null() {
            "NULL"
        } else {
            &self.raw
        }
    }

    /// The text an export writes where it has no way to mark a NULL: empty,
    /// so the string `NULL` stays distinguishable from a SQL NULL.
    pub(in crate::app) fn plain_text(&self) -> &str {
        if self.is_null() {
            ""
        } else {
            &self.raw
        }
    }

    /// The value as a SQL literal of `target`'s engine.
    pub(in crate::app) fn sql(&self, target: SqlTarget) -> Result<String, String> {
        form_literal(&self.form, &self.raw, target)
    }

    /// `name = literal`, or `name IS NULL`.
    fn condition(&self, target: SqlTarget, name: &str) -> Result<String, String> {
        let name = target.quote(name)?;
        Ok(if self.is_null() {
            format!("{name} IS NULL")
        } else {
            format!("{name} = {}", self.sql(target)?)
        })
    }
}

/// The literal of a value in `form`.
fn form_literal(form: &SqlForm, raw: &str, target: SqlTarget) -> Result<String, String> {
    Ok(match form {
        SqlForm::Null => "NULL".into(),
        SqlForm::Boolean(value) => if *value { "TRUE" } else { "FALSE" }.into(),
        SqlForm::Number => raw.trim().to_string(),
        SqlForm::Text => sql_string_literal(target.dialect, raw),
        SqlForm::Binary => {
            let trimmed = raw.trim();
            let hex = trimmed
                .strip_prefix("0x")
                .or_else(|| trimmed.strip_prefix("\\x"))
                .unwrap_or(trimmed);
            target.binary.literal(hex)
        }
        SqlForm::Unwritable(reason) => return Err((*reason).to_string()),
    })
}

/// The SQL literal of a cell whose text is `raw` in a column of type `data_type` (`None`: a SQL NULL), without the
/// cell value a copy keeps: for a stream of rows.
pub(in crate::app) fn sql_literal(raw: Option<&str>, data_type: &str, target: SqlTarget) -> Result<String, String> {
    match raw {
        None => Ok("NULL".into()),
        Some(raw) => form_literal(&CopyCell::sql_form(raw, data_type, false), raw, target),
    }
}

/// The engine a copied SQL text is written for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::app) struct SqlTarget {
    pub(in crate::app) dialect: SqlDialect,
    pub(in crate::app) binary: BinaryLiteral,
}

impl SqlTarget {
    #[cfg(test)]
    pub(in crate::app) const POSTGRES: Self = Self {
        dialect: SqlDialect::POSTGRES,
        binary: BinaryLiteral::Postgres,
    };

    pub(in crate::app) fn quote(self, name: &str) -> Result<String, String> {
        if name.is_empty() || name.contains('\0') {
            return Err("Invalid SQL identifier".into());
        }
        self.dialect.checked_quote(name).map_err(|error| error.to_string())
    }
}

pub(in crate::app) struct CopyData {
    pub(in crate::app) columns: Vec<String>,
    pub(in crate::app) rows: Vec<Vec<CopyCell>>,
    pub(in crate::app) table: String,
    pub(in crate::app) schema: Option<String>,
    pub key_column: Option<String>,
    pub keys: Vec<CopyCell>,
    pub(in crate::app) target: SqlTarget,
}

/// `"column" = literal` (or `IS NULL`) for the cell-detail "Copy SQL Condition"
/// action, in the quoting and literal syntax of `target`'s engine.
pub(in crate::app) fn sql_condition(
    target: SqlTarget,
    column: &str,
    cell: &CopyCell,
) -> Result<String, String> {
    cell.condition(target, column)
}

/// The names of `columns` with each repeat made unique in order (`id`, `id_2`,
/// `id_3`), and the renames made as `(original, new)`. A generated name never
/// equals a real column's name or another generated one.
pub(in crate::app) fn unique_column_names<'a>(
    columns: impl IntoIterator<Item = &'a str>,
) -> (Vec<String>, Vec<(String, String)>) {
    let columns: Vec<&str> = columns.into_iter().collect();
    let mut taken: HashSet<String> = columns.iter().map(|name| name.to_string()).collect();
    let mut seen: HashSet<&str> = HashSet::with_capacity(columns.len());
    let mut renames = Vec::new();
    let names = columns
        .iter()
        .map(|name| {
            if seen.insert(name) {
                return name.to_string();
            }
            let renamed = (2usize..)
                .map(|number| format!("{name}_{number}"))
                .find(|candidate| !taken.contains(candidate))
                .expect("an unbounded range");
            taken.insert(renamed.clone());
            renames.push((name.to_string(), renamed.clone()));
            renamed
        })
        .collect();
    (names, renames)
}

/// What the status row says after a JSON text was made with renamed columns.
pub(in crate::app) fn renamed_columns_notice(renames: &[(String, String)]) -> Option<String> {
    let first = renames.first()?;
    let more = renames.len() - 1;
    Some(format!(
        "Repeated column names were made unique in the JSON: {} became {}{}",
        first.0,
        first.1,
        if more > 0 {
            format!(" (and {more} more)")
        } else {
            String::new()
        }
    ))
}

impl CopyFormat {
    pub(in crate::app) fn is_json(self) -> bool {
        matches!(self, Self::Json | Self::JsonLines | Self::Pretty)
    }
}

fn xml_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

impl CopyData {
    fn qualified_sql_table(&self) -> Result<String, String> {
        let schema = self
            .schema
            .as_deref()
            .ok_or("SQL table metadata is unavailable")?;
        Ok(format!(
            "{}.{}",
            self.target.quote(schema)?,
            self.target.quote(&self.table)?
        ))
    }

    /// Unique column names for the JSON formats, and the renames made.
    pub(in crate::app) fn json_columns(&self) -> (Vec<String>, Vec<(String, String)>) {
        unique_column_names(self.columns.iter().map(String::as_str))
    }

    pub(in crate::app) fn format_with_dsv(
        &self,
        format: CopyFormat,
        options: &DsvOptions,
    ) -> Result<String, String> {
        if format != CopyFormat::Dsv {
            return self.format(format);
        }
        options.validate()?;
        if self.columns.is_empty()
            || self.rows.is_empty()
            || self.rows.iter().any(|row| row.len() != self.columns.len())
        {
            return Err("No rectangular selection to copy".into());
        }
        let mut lines = Vec::with_capacity(self.rows.len() + usize::from(options.include_headers));
        if options.include_headers {
            lines.push(
                self.columns
                    .iter()
                    .map(|name| options.field(name))
                    .collect::<Vec<_>>()
                    .join(&options.column_separator),
            );
        }
        for row in &self.rows {
            lines.push(
                row.iter()
                    .map(|cell| options.cell(cell))
                    .collect::<Vec<_>>()
                    .join(&options.column_separator),
            );
        }
        let result = lines.join(&options.row_separator);
        if result.len() > COPY_MAX_BYTES {
            return Err("Formatted copy exceeds the 16 MiB limit".into());
        }
        Ok(result)
    }

    fn json_rows(&self) -> Vec<serde_json::Value> {
        let (names, _) = self.json_columns();
        self.rows
            .iter()
            .map(|row| {
                let mut map = serde_json::Map::new();
                for (name, cell) in names.iter().zip(row).filter(|(_, cell)| !cell.missing) {
                    map.insert(name.clone(), cell.value.clone());
                }
                serde_json::Value::Object(map)
            })
            .collect()
    }

    fn where_clause(&self) -> Result<String, String> {
        let mut clauses = Vec::with_capacity(self.rows.len());
        for row in &self.rows {
            let terms = self
                .columns
                .iter()
                .zip(row)
                .map(|(column, cell)| cell.condition(self.target, column))
                .collect::<Result<Vec<_>, _>>()?;
            clauses.push(format!("({})", terms.join(" AND ")));
        }
        Ok(clauses.join(" OR "))
    }

    pub(in crate::app) fn format(&self, format: CopyFormat) -> Result<String, String> {
        if format == CopyFormat::Dsv {
            return self.format_with_dsv(format, &DsvOptions::default());
        }
        if self.columns.is_empty()
            || self.rows.is_empty()
            || self.rows.iter().any(|row| row.len() != self.columns.len())
        {
            return Err("No rectangular selection to copy".into());
        }
        let bytes = self.rows.iter().flatten().try_fold(0usize, |total, cell| {
            total
                .checked_add(cell.raw.len())
                .filter(|size| *size <= COPY_MAX_BYTES)
                .ok_or("Selected values exceed the 16 MiB copy limit")
        })?;
        let format = if format == CopyFormat::Smart {
            if self.rows.len() == 1 && self.columns.len() == 1 {
                CopyFormat::Raw
            } else {
                CopyFormat::Tsv
            }
        } else {
            format
        };
        if matches!(
            format,
            CopyFormat::SqlIn
                | CopyFormat::SqlInsert
                | CopyFormat::SqlInsertNoPk
                | CopyFormat::SqlUpdate
                | CopyFormat::SqlSelect
                | CopyFormat::SqlWhere
        ) {
            self.qualified_sql_table()?;
        }
        let target = self.target;
        let output = match format {
            CopyFormat::Raw => {
                if self.rows.len() != 1 || self.columns.len() != 1 {
                    return Err("Raw copy needs one cell".into());
                }
                self.rows[0][0].plain_text().to_string()
            }
            CopyFormat::Tsv
            | CopyFormat::TsvWithHeaders
            | CopyFormat::Csv
            | CopyFormat::CsvWithHeaders
            | CopyFormat::Pipe
            | CopyFormat::Dsv
            | CopyFormat::OneRow => {
                if format == CopyFormat::OneRow && self.rows.len() != 1 {
                    return Err("One-row copy needs one selected row".into());
                }
                let sep = match format {
                    CopyFormat::Csv | CopyFormat::CsvWithHeaders | CopyFormat::Dsv => ',',
                    CopyFormat::Pipe => '|',
                    _ => '\t',
                };
                let headers = matches!(
                    format,
                    CopyFormat::TsvWithHeaders | CopyFormat::CsvWithHeaders
                );
                return self
                    .format_with_dsv(CopyFormat::Dsv, &DsvOptions::single(sep, headers))
                    .and_then(|text| {
                        if bytes > COPY_MAX_BYTES {
                            Err("Formatted copy exceeds the 16 MiB limit".into())
                        } else {
                            Ok(text)
                        }
                    });
            }
            CopyFormat::Json => {
                serde_json::to_string(&self.json_rows()).map_err(|e| e.to_string())?
            }
            CopyFormat::JsonLines => self
                .json_rows()
                .iter()
                .map(|value| serde_json::to_string(value).map_err(|e| e.to_string()))
                .collect::<Result<Vec<_>, _>>()?
                .join("\n"),
            CopyFormat::Pretty => {
                serde_json::to_string_pretty(&self.json_rows()).map_err(|e| e.to_string())?
            }
            CopyFormat::SqlIn => {
                if self.columns.len() != 1 {
                    return Err("SQL IN requires exactly one selected column".into());
                }
                format!(
                    "({})",
                    self.rows
                        .iter()
                        .map(|row| row[0].sql(target))
                        .collect::<Result<Vec<_>, _>>()?
                        .join(", ")
                )
            }
            CopyFormat::SqlInsert | CopyFormat::SqlInsertNoPk => {
                let selected = self
                    .columns
                    .iter()
                    .enumerate()
                    .filter(|(_, name)| {
                        format != CopyFormat::SqlInsertNoPk
                            || self.key_column.as_ref() != Some(*name)
                    })
                    .collect::<Vec<_>>();
                if selected.is_empty() {
                    return Err("No insertable columns selected".into());
                }
                let names = selected
                    .iter()
                    .map(|(_, name)| target.quote(name))
                    .collect::<Result<Vec<_>, _>>()?
                    .join(", ");
                let values = self
                    .rows
                    .iter()
                    .map(|row| {
                        let literals = selected
                            .iter()
                            .map(|(index, _)| row[*index].sql(target))
                            .collect::<Result<Vec<_>, _>>()?;
                        Ok(format!("({})", literals.join(", ")))
                    })
                    .collect::<Result<Vec<_>, String>>()?
                    .join(",\n");
                format!(
                    "INSERT INTO {} ({names}) VALUES\n{values};",
                    self.qualified_sql_table()?
                )
            }
            CopyFormat::SqlUpdate => {
                let key = self
                    .key_column
                    .as_deref()
                    .ok_or("SQL UPDATE requires a primary key")?;
                if self.keys.len() != self.rows.len() {
                    return Err("Primary key values are unavailable".into());
                }
                let updates = self
                    .rows
                    .iter()
                    .zip(&self.keys)
                    .map(|(row, key_value)| {
                        let assignments = self
                            .columns
                            .iter()
                            .zip(row)
                            .filter(|(name, _)| name.as_str() != key)
                            .map(|(name, cell)| {
                                Ok(format!("{} = {}", target.quote(name)?, cell.sql(target)?))
                            })
                            .collect::<Result<Vec<_>, String>>()?;
                        if assignments.is_empty() {
                            return Err("No updatable columns selected".into());
                        }
                        Ok(format!(
                            "UPDATE {} SET {} WHERE {};",
                            self.qualified_sql_table()?,
                            assignments.join(", "),
                            key_value.condition(target, key)?
                        ))
                    })
                    .collect::<Result<Vec<_>, String>>()?;
                updates.join("\n")
            }
            CopyFormat::SqlWhere => self.where_clause()?,
            CopyFormat::SqlSelect => format!(
                "SELECT * FROM {} WHERE {};",
                self.qualified_sql_table()?,
                self.where_clause()?
            ),
            CopyFormat::Markdown => {
                let escaped = |text: &str| {
                    text.replace('\\', "\\\\")
                        .replace('|', "\\|")
                        .replace('\n', "<br>")
                };
                let header = format!(
                    "| {} |",
                    self.columns
                        .iter()
                        .map(|name| escaped(name))
                        .collect::<Vec<_>>()
                        .join(" | ")
                );
                let divider = format!("| {} |", vec!["---"; self.columns.len()].join(" | "));
                let rows = self
                    .rows
                    .iter()
                    .map(|row| {
                        format!(
                            "| {} |",
                            row.iter()
                                .map(|cell| escaped(cell.plain_text()))
                                .collect::<Vec<_>>()
                                .join(" | ")
                        )
                    })
                    .collect::<Vec<_>>();
                std::iter::once(header)
                    .chain(std::iter::once(divider))
                    .chain(rows)
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            CopyFormat::Html => {
                let mut html = String::from("<table><thead><tr>");
                for name in &self.columns {
                    html.push_str(&format!("<th>{}</th>", xml_escape(name)));
                }
                html.push_str("</tr></thead><tbody>");
                for row in &self.rows {
                    html.push_str("<tr>");
                    for cell in row {
                        html.push_str(&format!("<td>{}</td>", xml_escape(cell.plain_text())));
                    }
                    html.push_str("</tr>");
                }
                html.push_str("</tbody></table>");
                html
            }
            // A SQL NULL is an empty element, as the XML export writes
            // it: the string `NULL` stays distinguishable.
            CopyFormat::Xml => {
                let mut xml = String::from("<rows>");
                for row in &self.rows {
                    xml.push_str("<row>");
                    for (name, cell) in self.columns.iter().zip(row) {
                        xml.push_str(&format!(
                            "<field name=\"{}\">{}</field>",
                            xml_escape(name),
                            xml_escape(cell.plain_text())
                        ));
                    }
                    xml.push_str("</row>");
                }
                xml.push_str("</rows>");
                xml
            }
            CopyFormat::Smart => unreachable!(),
        };
        if output.len() > COPY_MAX_BYTES || bytes > COPY_MAX_BYTES {
            return Err("Formatted copy exceeds the 16 MiB limit".into());
        }
        Ok(output)
    }
}

/// A copied text, and a note for the status row when the copy had to change
/// something the user should know about.
pub(in crate::app) struct CopyOutput {
    pub(in crate::app) text: String,
    pub(in crate::app) notice: Option<String>,
}

impl CopyOutput {
    fn text(text: String) -> Self {
        Self { text, notice: None }
    }
}

impl AppRoot {
    #[cfg(test)]
    pub(in crate::app) fn copy_context_selection(
        &self,
        tab_id: u64,
        format: CopyFormat,
    ) -> Result<String, String> {
        self.copy_context_selection_output(tab_id, format)
            .map(|output| output.text)
    }

    fn copy_context_selection_output(
        &self,
        tab_id: u64,
        format: CopyFormat,
    ) -> Result<CopyOutput, String> {
        let range = self
            .view(tab_id)
            .grid_ui
            .selection
            .borrow()
            .range
            .ok_or("Select cells first")?
            .normalized();
        self.copy_context_range(tab_id, range, format)
    }

    #[cfg(test)]
    pub(in crate::app) fn copy_context_scope(
        &self,
        tab_id: u64,
        scope: CopyScope,
        format: CopyFormat,
    ) -> Result<String, String> {
        self.copy_context_scope_output(tab_id, scope, format)
            .map(|output| output.text)
    }

    fn copy_context_scope_output(
        &self,
        tab_id: u64,
        scope: CopyScope,
        format: CopyFormat,
    ) -> Result<CopyOutput, String> {
        let view = self.view(tab_id);
        let selected = view
            .grid_ui
            .selection
            .borrow()
            .range
            .map(|range| range.normalized());
        let target = view
            .grid_ui
            .context_cell
            .or_else(|| selected.map(|range| range.start))
            .ok_or("Select a cell first")?;
        let max_row = self
            .grid(tab_id)
            .row_count()
            .checked_sub(1)
            .ok_or("No rows to copy")?;
        let max_column = self
            .display_column_count(tab_id)
            .checked_sub(1)
            .ok_or("No columns to copy")?;
        let range = match scope {
            CopyScope::Selected => selected.ok_or("Select cells first")?,
            CopyScope::Cell => crate::grid::CellRange {
                start: target,
                end: target,
            },
            CopyScope::Rows => {
                let selected = selected.ok_or("Select rows first")?;
                crate::grid::CellRange {
                    start: CellPosition {
                        row: selected.start.row,
                        column: 0,
                    },
                    end: CellPosition {
                        row: selected.end.row,
                        column: max_column,
                    },
                }
            }
            CopyScope::Page => crate::grid::CellRange {
                start: CellPosition { row: 0, column: 0 },
                end: CellPosition {
                    row: max_row,
                    column: max_column,
                },
            },
            CopyScope::ColumnNames => {
                let selected = selected.ok_or("Select columns first")?;
                return (selected.start.column..=selected.end.column)
                    .map(|column| {
                        self.source_column_for_display(tab_id, column)
                            .and_then(|source| self.grid(tab_id).column_name(source))
                            .ok_or("Column unavailable")
                    })
                    .collect::<Result<Vec<_>, _>>()
                    .map(|names| CopyOutput::text(names.join("\t")))
                    .map_err(str::to_string);
            }
        };
        if matches!(scope, CopyScope::Rows)
            && format == CopyFormat::Smart
            && self.grid_prefs.copy_default == CopyFormat::Smart
        {
            let data = self.collect_copy_context_range(tab_id, range)?;
            let notice = renamed_columns_notice(&data.json_columns().1);
            let rows = data.json_rows();
            let json = if rows.len() == 1 {
                serde_json::to_string_pretty(&rows[0])
            } else {
                serde_json::to_string_pretty(&rows)
            }
            .map_err(|error| error.to_string())?;
            if json.len() > COPY_MAX_BYTES {
                return Err("Formatted copy exceeds 16 MiB".into());
            }
            return Ok(CopyOutput { text: json, notice });
        }
        let format = if matches!(scope, CopyScope::Page)
            && format == CopyFormat::Smart
            && self.grid_prefs.copy_default == CopyFormat::Smart
        {
            CopyFormat::TsvWithHeaders
        } else {
            format
        };
        self.copy_context_range(tab_id, range, format)
    }

    fn copy_context_range(
        &self,
        tab_id: u64,
        range: crate::grid::CellRange,
        format: CopyFormat,
    ) -> Result<CopyOutput, String> {
        let tab = self.workspace.tab(tab_id).ok_or("No result tab")?;
        let range = range.normalized();
        if range.end.row >= self.grid(tab_id).row_count()
            || range.end.column >= self.display_column_count(tab_id)
        {
            return Err("Selected cells are outside the current page".into());
        }
        let format = if format == CopyFormat::Smart {
            self.grid_prefs.copy_default
        } else {
            format
        };
        let sql_format = matches!(
            format,
            CopyFormat::SqlIn
                | CopyFormat::SqlInsert
                | CopyFormat::SqlInsertNoPk
                | CopyFormat::SqlUpdate
                | CopyFormat::SqlWhere
                | CopyFormat::SqlSelect
        );
        if sql_format {
            let column = self
                .source_column_for_display(tab_id, range.start.column)
                .and_then(|source| self.grid(tab_id).column_name(source))
                .ok_or("SQL source column unavailable")?;
            self.validate_sql_copy_context(tab_id, column)?;
            if format == CopyFormat::SqlInsertNoPk && self.single_sql_primary_key(tab.id).is_err() {
                return Err(
                    "SQL INSERT without PK requires exactly one identifiable primary key".into(),
                );
            }
        }
        let data = self.collect_copy_context_range(tab_id, range)?;
        let text = data.format(format)?;
        let notice = format
            .is_json()
            .then(|| renamed_columns_notice(&data.json_columns().1))
            .flatten();
        Ok(CopyOutput { text, notice })
    }

    pub(in crate::app) fn validate_sql_copy_context(
        &self,
        tab_id: u64,
        column: &str,
    ) -> Result<(), String> {
        let tab = self.workspace.tab(tab_id).ok_or("No active result tab")?;
        if !self.sql_sort_available(tab_id) || tab.collection.is_empty() {
            return Err("SQL copy requires a connected, unambiguous table result".into());
        }
        super::context_menu::sorted_sql_query(
            self.sql_table_dialect(tab),
            &tab.sql,
            &self.sql_row_schema(tab),
            &tab.collection,
            column,
            false,
        )
        .map_err(|error| error.to_string())?;
        Ok(())
    }

    pub(in crate::app) fn collect_copy_context_range(
        &self,
        tab_id: u64,
        range: crate::grid::CellRange,
    ) -> Result<CopyData, String> {
        let tab = self.workspace.tab(tab_id).ok_or("No result tab")?;
        let grid = self.grid(tab_id);
        let range = range.normalized();
        if range.end.row >= grid.row_count() || range.end.column >= self.display_column_count(tab_id)
        {
            return Err("Selected cells are outside the current page".into());
        }
        let source_columns = (range.start.column..=range.end.column)
            .map(|column| {
                self.source_column_for_display(tab_id, column)
                    .ok_or("Column unavailable")
            })
            .collect::<Result<Vec<_>, _>>()?;
        let columns = source_columns
            .iter()
            .map(|column| {
                grid.column_name(*column)
                    .map(str::to_string)
                    .ok_or("Column unavailable")
            })
            .collect::<Result<Vec<_>, _>>()?;
        let primary_key = self.single_sql_primary_key(tab.id).ok();
        let key_column = primary_key.map(|column| column.name.clone());
        let source_key = key_column.as_ref().and_then(|name| {
            (0..grid.column_count()).find(|index| grid.column_name(*index) == Some(name))
        });
        let sql = !tab.query_mode.is_mongo_find();
        let mut keys = Vec::with_capacity(range.end.row - range.start.row + 1);
        let mut rows = Vec::with_capacity(keys.capacity());
        let mut total = 0usize;
        for row in range.start.row..=range.end.row {
            if let (Some(pk), Some(column)) = (primary_key, source_key) {
                let position = CellPosition { row, column };
                let key_text = grid
                    .full_cell_text(position)
                    .ok_or("Primary key unavailable")?;
                keys.push(self.copy_cell(
                    tab.id,
                    sql,
                    (row, column),
                    &key_text,
                    &pk.data_type,
                    grid.cell_is_null(position),
                ));
            }
            let mut cells = Vec::with_capacity(columns.len());
            for (display_column, source_column) in
                (range.start.column..=range.end.column).zip(&source_columns)
            {
                let position = CellPosition {
                    row,
                    column: *source_column,
                };
                let display_position = CellPosition {
                    row,
                    column: display_column,
                };
                // A field the document lacks, or one a staged `$unset` removes,
                // is a blank cell and copies as an empty string (a BSON null is
                // still NULL). The "(unset)" shown in the cell is not its value.
                let unset = !sql
                    && self.workspace.runtime(tab.id).is_some_and(|runtime| {
                        runtime.mongo.mutations.is_unset(row, &columns[display_column - range.start.column])
                    });
                let found = if unset {
                    None
                } else {
                    self.pending_cell_text(tab.id, display_position)
                        .map(|text| text.to_string())
                        .or_else(|| grid.full_cell_text(position))
                };
                let missing = !sql && found.is_none();
                let raw = found
                    .or_else(|| missing.then(String::new))
                    .ok_or("Full selected cell value unavailable")?;
                total = total
                    .checked_add(raw.len())
                    .filter(|bytes| *bytes <= COPY_MAX_BYTES)
                    .ok_or("Selection exceeds the 16 MiB copy limit")?;
                let pending_null = self
                    .workspace
                    .runtime(tab.id)
                    .and_then(|runtime| {
                        runtime
                            .pending_edits
                            .iter()
                            .find(|edit| edit.row == row && edit.column == display_column)
                    })
                    .map(|edit| matches!(edit.request.value, MutationValue::Null));
                let mongo_null = self.workspace.runtime(tab.id).and_then(|runtime| {
                    match runtime
                        .mongo
                        .mutations
                        .latest_field_change(row, &columns[display_column - range.start.column])
                    {
                        Some(PendingMongoMutation::Update { value, .. }) => Some(value.is_null()),
                        Some(PendingMongoMutation::Unset { .. }) => Some(false),
                        _ => None,
                    }
                });
                let is_null = pending_null
                    .or(mongo_null)
                    .unwrap_or_else(|| grid.cell_is_null(position));
                let type_name = if sql {
                    self.sql_edit_data_type(tab.id, *source_column, row)
                } else {
                    grid.column_type(*source_column).unwrap_or("text").to_string()
                };
                let mut cell = self.copy_cell(
                    tab.id,
                    sql,
                    (row, *source_column),
                    &raw,
                    &type_name,
                    is_null && !missing,
                );
                cell.missing = missing;
                cells.push(cell);
            }
            rows.push(cells);
        }
        Ok(CopyData {
            columns,
            rows,
            table: tab.collection.clone(),
            schema: tab.query_mode.is_sql_table()
                .then(|| self.sql_row_schema(tab)),
            key_column,
            keys,
            target: self.sql_copy_target(tab),
        })
    }

    /// The cell at (display `row`, source column) as a copied cell. A SQL cell
    /// is read by the type its value really has; one the engine cannot take
    /// back as SQL text (MySQL geometry as WKT)
    /// refuses SQL formats and still copies as text.
    pub(in crate::app) fn copy_cell(
        &self,
        tab_id: u64,
        sql: bool,
        (row, source_column): (usize, usize),
        raw: &str,
        type_name: &str,
        is_null: bool,
    ) -> CopyCell {
        if sql && !is_null {
            if let Some(reason) = self
                .sql_cell_read_only_reason(tab_id, row, source_column)
                .filter(|reason| *reason != super::editing::INVALID_UTF8_REASON)
            {
                return CopyCell::unwritable(raw, type_name, reason);
            }
        }
        CopyCell::new(raw, type_name, is_null)
    }

    /// Quoting, string escaping and binary literals of the tab's connection.
    pub(in crate::app) fn sql_copy_target(&self, tab: &Tab) -> SqlTarget {
        SqlTarget {
            dialect: self.sql_table_dialect(tab),
            binary: self.binary_literal_for_connection(&tab.connection_id),
        }
    }

    pub(in crate::app) fn copy_format_to_clipboard(
        &mut self,
        tab_id: u64,
        format: CopyFormat,
        cx: &mut Context<Self>,
    ) {
        let result = self.copy_context_selection_output(tab_id, format);
        self.finish_copy(tab_id, result, cx);
    }

    pub(in crate::app) fn copy_scope_to_clipboard(
        &mut self,
        tab_id: u64,
        scope: CopyScope,
        format: CopyFormat,
        cx: &mut Context<Self>,
    ) {
        let result = self.copy_context_scope_output(tab_id, scope, format);
        self.finish_copy(tab_id, result, cx);
    }

    /// Writes a copy to the clipboard; the status row says what went wrong, or
    /// that column names were renamed in a JSON text.
    fn finish_copy(
        &mut self,
        tab_id: u64,
        result: Result<CopyOutput, String>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(output) => {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(output.text));
                if let Some(notice) = output.notice {
                    self.view_mut(tab_id).detail_ui.error = Some(notice);
                    self.notify_tab(tab_id, cx);
                }
            }
            Err(error) => {
                self.view_mut(tab_id).detail_ui.error = Some(error);
                self.notify_tab(tab_id, cx);
            }
        }
    }

    pub(in crate::app) fn set_copy_default(
        &mut self,
        tab_id: u64,
        format: CopyFormat,
        cx: &mut Context<Self>,
    ) {
        let result = self
            .grid_prefs
            .copy_default_path
            .clone()
            .ok_or("Copy preference directory unavailable".to_string())
            .and_then(|path| write_copy_default(&path, format));
        match result {
            Ok(()) => {
                self.grid_prefs.copy_default = format;
                // The setting follows the menu choice (when it offers it).
                if let Some(choice) = format.to_setting() {
                    let mut next = (*self.settings).clone();
                    next.results.default_copy_format = choice;
                    self.apply_settings(next, None, cx);
                }
            }
            Err(error) => {
                self.report_error(Area::App, None, &error);
                self.view_mut(tab_id).detail_ui.error = Some(error);
            }
        }
        // The default is shared by every tab's context menu.
        cx.notify();
    }

    pub(in crate::app) fn open_copy_dsv_dialog(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use gpui_component::select::{Select, SelectState};

        if self.view(tab_id).grid_ui.selection.borrow().range.is_none() {
            window.open_alert_dialog(cx, |dialog, _, _| {
                dialog
                    .title("Configure DSV copy")
                    .child("Select cells first")
            });
            return;
        }
        let column = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Column separator")
                .default_value(",")
        });
        let quote = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Quote character (one)")
                .default_value("\"")
        });
        let null = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("NULL text (empty: leave the field empty)")
                .default_value("")
        });
        let row_separator = cx.new(|cx| {
            SelectState::new(
                vec![
                    "Line feed (LF)".to_string(),
                    "Carriage return + line feed (CRLF)".to_string(),
                ],
                None,
                window,
                cx,
            )
        });
        let headers = cx.new(|cx| {
            SelectState::new(
                vec!["Without headers".to_string(), "With headers".to_string()],
                None,
                window,
                cx,
            )
        });
        let root = cx.entity().downgrade();
        let error = std::rc::Rc::new(std::cell::RefCell::new(None::<String>));
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let column = column.clone();
            let quote = quote.clone();
            let null = null.clone();
            let row_separator = row_separator.clone();
            let headers = headers.clone();
            let root = root.clone();
            let error = error.clone();
            let message = error.borrow().clone();
            dialog
                .width(px(360.0))
                .title("Configure DSV copy")
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(8.0))
                        .child(
                            div()
                                .debug_selector(|| "copy-dsv-column".into())
                                .child(Input::new(&column).appearance(true)),
                        )
                        .child(
                            div()
                                .debug_selector(|| "copy-dsv-row-separator".into())
                                .child(
                                    Select::new(&row_separator).placeholder("Row separator (LF)"),
                                ),
                        )
                        .child(
                            div()
                                .debug_selector(|| "copy-dsv-quote".into())
                                .child(Input::new(&quote).appearance(true)),
                        )
                        .child(
                            div()
                                .debug_selector(|| "copy-dsv-null".into())
                                .child(Input::new(&null).appearance(true)),
                        )
                        .child(
                            div()
                                .debug_selector(|| "copy-dsv-headers".into())
                                .child(Select::new(&headers).placeholder("Headers (excluded)")),
                        )
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(theme::text_muted())
                                .child("Settings validate against the copy controller; invalid input keeps this dialog open."),
                        ),
                )
                .when_some(message, |dialog, message| {
                    dialog.child(
                        div()
                            .debug_selector(|| "copy-dsv-error".into())
                            .text_color(theme::danger())
                            .child(message),
                    )
                })
                .button_props(
                    gpui_component::dialog::DialogButtonProps::default().ok_text("Copy"),
                )
                .confirm()
                .footer(
                    gpui_component::dialog::DialogFooter::new()
                        .child(
                            div().debug_selector(|| "copy-dsv-cancel".into()).child(
                                gpui_component::dialog::DialogClose::new()
                                    .trigger(|button| button.label("Cancel")),
                            ),
                        )
                        .child(
                            div().debug_selector(|| "copy-dsv-apply".into()).child(
                                gpui_component::dialog::DialogAction::new().child(
                                    Button::new("copy-dsv-apply-button").label("Copy").primary(),
                                ),
                            ),
                        ),
                )
                .on_ok(move |_, window, cx| {
                    let column_text = column.read(cx).value().to_string();
                    let quote_text = quote.read(cx).value().to_string();
                    let null_text = null.read(cx).value().to_string();
                    let row_text = row_separator
                        .read(cx)
                        .selected_value()
                        .cloned()
                        .unwrap_or_default();
                    let headers_text =
                        headers.read(cx).selected_value().cloned().unwrap_or_default();
                    let quote = {
                        let mut chars = quote_text.chars();
                        match (chars.next(), chars.next()) {
                            (Some(quote), None) if !quote.is_control() => quote,
                            _ => {
                                *error.borrow_mut() =
                                    Some("Quote must be one non-control character".into());
                                window.refresh();
                                return false;
                            }
                        }
                    };
                    let options = DsvOptions {
                        column_separator: column_text,
                        row_separator: if row_text.contains("CRLF") {
                            "\r\n".into()
                        } else {
                            "\n".into()
                        },
                        null_text,
                        quote,
                        include_headers: headers_text.contains("With headers"),
                    };
                    let result = root.update(cx, |this, cx| {
                        let range = this
                            .view(tab_id)
                            .grid_ui
                            .selection
                            .borrow()
                            .range
                            .ok_or("Select cells first")?;
                        let data = this.collect_copy_context_range(tab_id, range)?;
                        let text = data.format_with_dsv(CopyFormat::Dsv, &options)?;
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                        Ok::<_, String>(())
                    });
                    match result {
                        Ok(Ok(())) => true,
                        Ok(Err(message)) => {
                            *error.borrow_mut() = Some(message);
                            window.refresh();
                            false
                        }
                        Err(_) => false,
                    }
                })
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_formats_preserve_null_empty_native_values_and_escape_untrusted_text() {
        let data = CopyData {
            columns: vec!["id".into(), "na\"me".into()],
            rows: vec![
                vec![
                    CopyCell::new("7", "int64", false),
                    CopyCell::new("O'Brien,<tag>\n", "text", false),
                ],
                vec![
                    CopyCell::new("8", "int64", false),
                    CopyCell::new("", "text", true),
                ],
            ],
            table: "orders".into(),
            schema: Some("sales".into()),
            key_column: Some("id".into()),
            keys: vec![CopyCell::new("7", "int64", false), CopyCell::new("8", "int64", false)],
            target: SqlTarget::POSTGRES,
        };
        assert_eq!(
            data.format(CopyFormat::CsvWithHeaders).unwrap(),
            "id,\"na\"\"me\"\n7,\"O'Brien,<tag>\n\"\n8,"
        );
        let json: serde_json::Value =
            serde_json::from_str(&data.format(CopyFormat::Json).unwrap()).unwrap();
        assert_eq!(json[0]["id"], 7);
        assert_eq!(json[1]["na\"me"], serde_json::Value::Null);
        let insert = data.format(CopyFormat::SqlInsert).unwrap();
        assert!(insert.contains("INSERT INTO \"sales\".\"orders\""));
        assert!(insert.contains("'O''Brien,<tag>"));
        assert!(data
            .format(CopyFormat::SqlUpdate)
            .unwrap()
            .contains("UPDATE \"sales\".\"orders\""));
        assert!(data
            .format(CopyFormat::SqlSelect)
            .unwrap()
            .contains("FROM \"sales\".\"orders\""));
        assert!(data
            .format(CopyFormat::Html)
            .unwrap()
            .contains("O&#39;Brien,&lt;tag&gt;"));
        assert!(data
            .format(CopyFormat::Xml)
            .unwrap()
            .contains("&lt;tag&gt;"));
        assert!(data.format(CopyFormat::SqlIn).is_err());
        let missing_metadata = CopyData {
            schema: None,
            ..data
        };
        assert!(missing_metadata.format(CopyFormat::SqlInsert).is_err());
        assert!(missing_metadata.format(CopyFormat::SqlUpdate).is_err());
        assert!(missing_metadata.format(CopyFormat::SqlSelect).is_err());
    }

    #[test]
    fn copy_output_limit_rejects_instead_of_truncating() {
        let data = CopyData {
            columns: vec!["x".into()],
            rows: vec![vec![CopyCell::new(
                &"x".repeat(COPY_MAX_BYTES + 1),
                "text",
                false,
            )]],
            table: "t".into(),
            schema: None,
            key_column: None,
            keys: vec![],
            target: SqlTarget::POSTGRES,
        };
        assert!(data.format(CopyFormat::Raw).is_err());
    }

    #[test]
    fn configured_default_persists_independently_from_one_off_copy() {
        let path =
            std::env::temp_dir().join(format!("tablex-copy-default-{}.json", uuid::Uuid::new_v4()));
        assert_eq!(read_copy_default(&path), CopyFormat::Smart);
        write_copy_default(&path, CopyFormat::CsvWithHeaders).unwrap();
        assert_eq!(read_copy_default(&path), CopyFormat::CsvWithHeaders);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn decimal_json_copy_keeps_all_original_digits() {
        let data = CopyData {
            columns: vec!["amount".into()],
            rows: vec![vec![CopyCell::new(
                "12345678901234567890.25",
                "decimal128",
                false,
            )]],
            table: "orders".into(),
            schema: None,
            key_column: None,
            keys: vec![],
            target: SqlTarget::POSTGRES,
        };
        assert_eq!(
            data.format(CopyFormat::Json).unwrap(),
            "[{\"amount\":\"12345678901234567890.25\"}]"
        );
    }

    #[test]
    fn custom_dsv_escapes_quotes_separators_and_null_without_truncation() {
        let data = CopyData {
            columns: vec!["name".into(), "empty".into()],
            rows: vec![vec![
                CopyCell::new("a||b'c", "text", false),
                CopyCell::new("", "text", true),
            ]],
            table: "t".into(),
            schema: None,
            key_column: None,
            keys: vec![],
            target: SqlTarget::POSTGRES,
        };
        let options = DsvOptions {
            column_separator: "||".into(),
            row_separator: "\r\n".into(),
            null_text: "<null>".into(),
            quote: '\'',
            include_headers: true,
        };
        assert_eq!(
            data.format_with_dsv(CopyFormat::Dsv, &options).unwrap(),
            "name||empty\r\n'a||b''c'||<null>"
        );
        let invalid = DsvOptions {
            column_separator: String::new(),
            ..options
        };
        assert!(data.format_with_dsv(CopyFormat::Dsv, &invalid).is_err());
    }
}
