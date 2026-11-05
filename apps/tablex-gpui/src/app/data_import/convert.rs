//! A cell of the file as the value of the column it goes to: the JSON the driver binds as a parameter.
//!
//! The kind of a target column comes from the same resolver the cell editor uses (`value_kind`), and so do
//! the number rules (`mutation_value_from_cell`, `mutation_value_to_json_for`): an import writes exactly
//! what typing the text into the cell would. What an import adds is the spelling hints of the file (a decimal
//! comma, a date format), the boolean words a file uses (`yes`, `no`, `on`) and a check of dates, so the preview
//! can flag a value the server would refuse.

use serde_json::Value;

use super::super::results::column_type::{base_type_head, value_kind, ValueKind};
use super::super::results::editing::{mutation_value_from_cell, mutation_value_to_json_for, MutationInputError};
use crate::import::infer::Hints;
use crate::staged_sql::MutationValue;

/// How the text for a column is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Integer,
    Decimal,
    Float,
    Boolean,
    /// `tinyint(1)` and `bit(1)`: a flag, or a small whole number.
    Flag,
    Binary,
    /// PostgreSQL `bytea`: its text form is `\x` and hex digits, which is also what the cell editor shows and takes.
    Bytea,
    Date,
    Timestamp,
    /// Text, JSON, UUID, time, enums and everything else: the server reads the text.
    Text,
}

/// Why a cell is not a value of its column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum ConvertError {
    Input(MutationInputError),
    NotADate,
    NotATimestamp,
}

impl std::fmt::Display for ConvertError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Input(MutationInputError::Integer) => "is not a whole number",
            Self::Input(MutationInputError::Decimal | MutationInputError::Float) => "is not a number",
            Self::Input(MutationInputError::Boolean) => "is not a boolean (true, false, t, f, yes, no, on, off, 1 or 0)",
            Self::Input(MutationInputError::Binary) => "is not binary (0x followed by an even number of hex digits)",
            Self::NotADate => "is not a date in the format of the file",
            Self::NotATimestamp => "is not a timestamp in the format of the file",
        })
    }
}

/// Reads the text for one target column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) struct Converter {
    data_type: String,
    kind: Kind,
    /// The column keeps the zone of a timestamp (`timestamptz`, `timestamp with time zone`, `DateTime64(6, 'UTC')`).
    zoned: bool,
}

impl Converter {
    pub(in crate::app) fn new(data_type: &str) -> Self {
        let head = base_type_head(data_type);
        let lower = data_type.to_ascii_lowercase().replace(' ', "");
        let kind = match value_kind(data_type) {
            ValueKind::Integer if lower.starts_with("tinyint(1)") => Kind::Flag,
            ValueKind::Integer => Kind::Integer,
            ValueKind::Decimal => Kind::Decimal,
            ValueKind::Float => Kind::Float,
            ValueKind::Boolean => Kind::Boolean,
            ValueKind::Binary => Kind::Binary,
            ValueKind::Temporal => match head.as_str() {
                "date" | "date32" => Kind::Date,
                "time" | "timetz" => Kind::Text,
                _ => Kind::Timestamp,
            },
            ValueKind::Text | ValueKind::Json if head == "bit" && lower.starts_with("bit(1)") => Kind::Flag,
            ValueKind::Json | ValueKind::Text if head == "bytea" => Kind::Bytea,
            ValueKind::Json | ValueKind::Text => Kind::Text,
        };
        let zoned = lower.contains("tz") || (lower.contains("timezone") && !lower.contains("without")) || lower.contains('\'');
        Self { data_type: data_type.to_owned(), kind, zoned }
    }

    /// `\x` + hex for a `bytea` column, from `\x` + hex or `0x` + hex (the form of the other databases). Anything else is
    /// refused: text sent as it is would be stored as its own ASCII bytes.
    fn bytea(text: &str) -> Option<String> {
        let text = text.trim();
        let digits = text.strip_prefix("\\x").or_else(|| text.strip_prefix("0x"))?;
        (digits.len() % 2 == 0 && digits.bytes().all(|byte| byte.is_ascii_hexdigit())).then(|| format!("\\x{digits}"))
    }

    /// The hex digits of `\x` + hex (the way PostgreSQL shows a `bytea`).
    fn pg_hex(text: &str) -> Option<&str> {
        let digits = text.trim().strip_prefix("\\x")?;
        (digits.len() % 2 == 0 && digits.bytes().all(|byte| byte.is_ascii_hexdigit())).then_some(digits)
    }

    /// `true`/`false` for the words a file spells a flag with.
    fn flag(text: &str) -> Option<bool> {
        match text.trim().to_ascii_lowercase().as_str() {
            "true" | "t" | "yes" | "y" | "on" | "1" => Some(true),
            "false" | "f" | "no" | "n" | "off" | "0" => Some(false),
            _ => None,
        }
    }

    /// The value `text` is in this column.
    pub(in crate::app) fn convert(&self, text: &str, hints: &Hints) -> Result<Value, ConvertError> {
        let number = |text: &str| match hints.number(text) {
            Some(plain) => mutation_value_from_cell(&plain, &self.data_type).map_err(ConvertError::Input),
            None => Err(ConvertError::Input(match self.kind {
                Kind::Integer | Kind::Flag => MutationInputError::Integer,
                Kind::Float => MutationInputError::Float,
                _ => MutationInputError::Decimal,
            })),
        };
        let value = match self.kind {
            Kind::Integer | Kind::Decimal | Kind::Float => number(text)?,
            Kind::Boolean => MutationValue::Boolean(Self::flag(text).ok_or(ConvertError::Input(MutationInputError::Boolean))?),
            Kind::Flag => match Self::flag(text) {
                Some(flag) => MutationValue::Boolean(flag),
                None => number(text).map_err(|_| ConvertError::Input(MutationInputError::Boolean))?,
            },
            Kind::Binary => {
                // PostgreSQL spells bytes `\x` + hex: the same bytes as `0x` + hex.
                let respelled = Self::pg_hex(text).map(|digits| format!("0x{digits}"));
                mutation_value_from_cell(respelled.as_deref().unwrap_or(text), &self.data_type).map_err(ConvertError::Input)?
            }
            Kind::Bytea => MutationValue::Text(Self::bytea(text).ok_or(ConvertError::Input(MutationInputError::Binary))?),
            Kind::Date => match hints.date(text) {
                Some(date) => MutationValue::Text(date),
                None if hints.passthrough_temporal => MutationValue::Text(text.to_owned()),
                None => return Err(ConvertError::NotADate),
            },
            Kind::Timestamp => {
                let timestamp = match hints.timestamp(text) {
                    Some(timestamp) => timestamp,
                    None if hints.passthrough_temporal => return Ok(mutation_value_to_json_for(&MutationValue::Text(text.to_owned()), &self.data_type)),
                    None => return Err(ConvertError::NotATimestamp),
                };
                // A column without a zone gets the instant as the UTC wall clock, not an offset it may refuse.
                let naive = (hints.offsets_to_utc && !self.zoned).then(|| Hints::utc_naive(&timestamp)).flatten();
                MutationValue::Text(naive.unwrap_or(timestamp))
            }
            Kind::Text => MutationValue::Text(text.to_owned()),
        };
        Ok(mutation_value_to_json_for(&value, &self.data_type))
    }

    /// An empty cell of a column that is not text is NULL where the column allows it (as in the cell editor): an
    /// empty text is a value, but an empty number, date or byte string is nothing.
    pub(in crate::app) fn empty_is_null(&self) -> bool {
        !matches!(self.kind, Kind::Text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn convert(data_type: &str, text: &str) -> Result<Value, String> {
        Converter::new(data_type).convert(text, &Hints::default()).map_err(|error| error.to_string())
    }

    #[test]
    fn numbers_keep_the_exact_text_the_cell_editor_would_send() {
        assert_eq!(convert("bigint", "42"), Ok(json!(42)));
        assert_eq!(convert("integer", " -7 "), Ok(json!(-7)));
        assert_eq!(convert("integer", "007"), Ok(json!("007")));
        assert_eq!(convert("numeric(12,2)", "1234.50"), Ok(json!("1234.50")));
        assert_eq!(convert("numeric", "7"), Ok(json!("7")));
        assert_eq!(convert("double precision", "1.5e3"), Ok(json!("1.5e3")));
        assert_eq!(convert("REAL", "2.25"), Ok(json!("2.25")));
    }

    #[test]
    fn a_value_the_server_would_refuse_is_named() {
        assert_eq!(convert("integer", "12.5"), Err("is not a whole number".into()));
        assert_eq!(convert("integer", "abc"), Err("is not a whole number".into()));
        assert_eq!(convert("numeric", "1,5"), Err("is not a number".into()));
        assert_eq!(convert("date", "31/12/2024"), Err("is not a date in the format of the file".into()));
        assert_eq!(convert("timestamp", "soon"), Err("is not a timestamp in the format of the file".into()));
        assert!(convert("varbinary(16)", "xyz").is_err());
    }

    #[test]
    fn booleans_read_the_words_a_file_uses() {
        for (text, flag) in [("true", true), ("T", true), ("Yes", true), ("on", true), ("1", true), ("false", false), ("n", false), ("OFF", false), ("0", false)] {
            assert_eq!(convert("boolean", text), Ok(json!(flag)), "{text}");
        }
        assert!(convert("boolean", "maybe").is_err());
        // A MySQL flag column is a tinyint(1): words are flags, other whole numbers stay numbers.
        assert_eq!(convert("tinyint(1)", "true"), Ok(json!(true)));
        assert_eq!(convert("tinyint(1)", "2"), Ok(json!(2)));
        assert!(convert("tinyint(1)", "x").is_err());
        assert_eq!(convert("tinyint", "5"), Ok(json!(5)));
    }

    #[test]
    fn dates_and_timestamps_are_checked_and_follow_the_formats_of_the_file() {
        assert_eq!(convert("date", "2024-03-31"), Ok(json!("2024-03-31")));
        assert!(convert("date", "2024-02-30").is_err());
        assert_eq!(convert("timestamp with time zone", "2024-03-31T10:00:00+02:00"), Ok(json!("2024-03-31T10:00:00+02:00")));
        assert_eq!(convert("datetime", "2024-03-31 10:00:00"), Ok(json!("2024-03-31 10:00:00")));
        // A time of day is the server's to read.
        assert_eq!(convert("time", "10:15"), Ok(json!("10:15")));
        let hints = Hints { decimal_comma: true, date_format: Some("%d.%m.%Y".into()), timestamp_format: Some("%d.%m.%Y %H:%M".into()), ..Hints::default() };
        assert_eq!(Converter::new("date").convert("31.03.2024", &hints), Ok(json!("2024-03-31")));
        assert_eq!(Converter::new("timestamp").convert("31.03.2024 10:00", &hints), Ok(json!("2024-03-31 10:00:00")));
        assert_eq!(Converter::new("numeric(10,2)").convert("1.234,50", &hints), Ok(json!("1234.50")));
        assert_eq!(Converter::new("integer").convert("1.234", &hints), Ok(json!(1234)));
    }

    #[test]
    fn a_timestamp_with_an_offset_goes_to_a_column_without_a_zone_as_utc_when_asked_to() {
        let utc = Hints { offsets_to_utc: true, ..Hints::default() };
        let convert = |data_type: &str, text: &str| Converter::new(data_type).convert(text, &utc).map_err(|error| error.to_string());
        assert_eq!(convert("datetime(6)", "2024-02-29 13:14:15+02"), Ok(json!("2024-02-29 11:14:15")));
        assert_eq!(convert("timestamp", "2024-02-29 13:14:15.123456+00:00"), Ok(json!("2024-02-29 13:14:15.123456")));
        assert_eq!(convert("datetime", "2024-02-29T23:30:00-0130"), Ok(json!("2024-03-01 01:00:00")));
        assert_eq!(convert("DATETIME", "2024-02-29T10:00:00Z"), Ok(json!("2024-02-29 10:00:00")));
        // A column that keeps zones keeps the offset; a text without one is not changed.
        assert_eq!(convert("timestamptz", "2024-02-29 13:14:15+02"), Ok(json!("2024-02-29 13:14:15+02")));
        assert_eq!(convert("timestamp with time zone", "2024-02-29 13:14:15+02"), Ok(json!("2024-02-29 13:14:15+02")));
        assert_eq!(convert("DateTime64(6, 'UTC')", "2024-02-29 13:14:15+02"), Ok(json!("2024-02-29 13:14:15+02")));
        assert_eq!(convert("timestamp without time zone", "2024-02-29 13:14:15+02"), Ok(json!("2024-02-29 11:14:15")));
        assert_eq!(convert("datetime", "2024-02-29 13:14:15.5"), Ok(json!("2024-02-29 13:14:15.5")));
        // An import leaves the offset to the server, as before.
        assert_eq!(Converter::new("datetime").convert("2024-02-29 13:14:15+02", &Hints::default()), Ok(json!("2024-02-29 13:14:15+02")));
    }

    #[test]
    fn a_special_date_goes_through_unchanged_only_between_servers_of_one_dialect() {
        let same = Hints { passthrough_temporal: true, offsets_to_utc: true, ..Hints::default() };
        for (data_type, text) in [("date", "0000-00-00"), ("timestamp", "infinity"), ("datetime", "0000-00-00 00:00:00"), ("timestamptz", "-infinity"), ("timestamptz", "0044-03-15 BC"), ("timestamptz", "1900-01-01 00:00:00+05:53:28")] {
            assert_eq!(Converter::new(data_type).convert(text, &same), Ok(json!(text)), "{text}");
            assert!(Converter::new(data_type).convert(text, &Hints::default()).is_err(), "{text} is refused where the dialects differ");
        }
        // What does parse is still read the same way.
        assert_eq!(Converter::new("date").convert("2024-03-31", &same), Ok(json!("2024-03-31")));
        assert_eq!(Converter::new("datetime").convert("2024-02-29 13:14:15+02", &same), Ok(json!("2024-02-29 11:14:15")));
    }

    #[test]
    fn bytea_takes_hex_in_either_spelling_and_never_sends_raw_text() {
        assert_eq!(convert("bytea", "\\x00ff"), Ok(json!("\\x00ff")));
        assert_eq!(convert("bytea", "0x00FF"), Ok(json!("\\x00FF")), "0x becomes \\x");
        assert_eq!(convert("bytea", "0x"), Ok(json!("\\x")));
        for text in ["00ff", "hello", "0x0", "0xzz", "\\xabc"] {
            assert!(convert("bytea", text).is_err(), "{text} would be stored as ASCII text");
        }
    }

    #[test]
    fn a_binary_column_takes_the_postgresql_spelling_of_bytes_too() {
        assert_eq!(convert("blob", "\\x00ff"), convert("blob", "0x00ff"));
        assert_eq!(convert("varbinary(8)", "\\x"), convert("varbinary(8)", "0x"));
        assert!(convert("blob", "\\x0").is_err() && convert("blob", "\\xzz").is_err());
    }

    #[test]
    fn text_like_columns_take_the_text_as_it_is() {
        for data_type in ["text", "varchar(20)", "uuid", "jsonb", "citext", "enum('a','b')", "time"] {
            assert_eq!(convert(data_type, " keep  me "), Ok(json!(" keep  me ")), "{data_type}");
        }
        assert_eq!(convert("blob", "0x00ff"), Ok(json!("0x00ff")));
    }

    #[test]
    fn an_empty_cell_of_a_binary_column_is_null_not_a_refusal() {
        for data_type in ["blob", "varbinary(16)", "binary(4)", "bytea", "longblob"] {
            assert!(Converter::new(data_type).empty_is_null(), "{data_type}");
        }
        // The text itself is still checked.
        assert!(convert("blob", "zz").is_err());
    }

    #[test]
    fn only_columns_that_are_not_text_turn_an_empty_cell_into_null() {
        assert!(Converter::new("integer").empty_is_null());
        assert!(Converter::new("date").empty_is_null());
        assert!(!Converter::new("text").empty_is_null());
        assert!(!Converter::new("varchar(10)").empty_is_null());
    }
}
