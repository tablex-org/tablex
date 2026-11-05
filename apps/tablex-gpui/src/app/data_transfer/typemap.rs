//! Column types across dialects: the type a table created for a transfer gives a column, in the words of the target
//! server, from the type the source server reports for it.
//!
//! The mapping goes through a small set of families (what a column holds, with its size) instead of from every
//! dialect to every other, so a new dialect adds one reading and one writing. The defaults aim at not losing a
//! value: an unbounded text is `text` / `longtext`, a decimal keeps its precision up to what the target allows, a
//! timestamp keeps microseconds, an unsigned 64-bit integer becomes a decimal where the target has no such integer.
//! Within one dialect family (PostgreSQL to PostgreSQL, MySQL to MariaDB) the type is kept as the server reports it:
//! enums, arrays and user-defined types mean something there. Every type can be edited in the mapping afterwards.

use crate::connections::ConnectionKind;

/// What a column holds, as far as a table in another dialect needs to know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum Family {
    Integer { bits: u16, unsigned: bool },
    Decimal { precision: Option<u32>, scale: Option<u32> },
    Float { double: bool },
    Boolean,
    Date,
    Time,
    Timestamp { zoned: bool },
    Text { length: Option<u32>, fixed: bool },
    Binary,
    Json,
    Uuid,
    /// Arrays, enums, intervals, geometry, user-defined types: text is what travels across dialects.
    Other,
}

/// The dialects that share their type names.
pub(in crate::app) fn same_family(a: ConnectionKind, b: ConnectionKind) -> bool {
    use ConnectionKind::{Mariadb, Mysql};
    a == b || (matches!(a, Mysql | Mariadb) && matches!(b, Mysql | Mariadb))
}

/// `type` without `Nullable(…)` / `LowCardinality(…)` wrappers.
fn unwrapped(data_type: &str) -> &str {
    let mut text = data_type.trim();
    loop {
        let inner = ["nullable(", "lowcardinality("].iter().find_map(|wrapper| {
            let head = text.get(..wrapper.len())?;
            head.eq_ignore_ascii_case(wrapper).then(|| text[wrapper.len()..].trim_end().strip_suffix(')')).flatten()
        });
        match inner {
            Some(inner) => text = inner.trim(),
            None => return text,
        }
    }
}

/// The words of a type outside its parentheses (lower case) and the numbers inside them.
fn parts(data_type: &str) -> (Vec<String>, Vec<u32>, String) {
    let text = unwrapped(data_type).to_ascii_lowercase();
    let (before, rest) = match text.split_once('(') {
        Some((before, rest)) => (before.to_owned(), rest),
        None => (text.clone(), ""),
    };
    let (inside, after) = rest.rsplit_once(')').unwrap_or((rest, ""));
    let words = before.split_whitespace().chain(after.split_whitespace()).map(str::to_owned).collect();
    let numbers = inside.split(',').filter_map(|part| part.trim().parse().ok()).collect();
    (words, numbers, text)
}

/// What the server `kind` means by `data_type`.
pub(in crate::app) fn family_of(kind: ConnectionKind, data_type: &str) -> Family {
    let (words, numbers, text) = parts(data_type);
    if text.ends_with(']') || text.starts_with("array") || text.starts_with("map(") || text.starts_with("tuple(") || text.starts_with("enum") || text.starts_with("set(") {
        return Family::Other;
    }
    let unsigned = words.iter().any(|word| word == "unsigned");
    let has = |word: &str| words.iter().any(|candidate| candidate == word);
    let first = words.iter().find(|word| !matches!(word.as_str(), "unsigned" | "signed" | "zerofill")).map_or("", String::as_str);
    let precision = numbers.first().copied();
    let integer = |bits, unsigned| Family::Integer { bits, unsigned };
    if kind == ConnectionKind::Clickhouse {
        // ClickHouse names its integers by their width (`Int8` is a byte, PostgreSQL's `int8` eight bytes).
        let width = |digits: &str| digits.parse::<u16>().ok();
        match first.strip_prefix("uint").map(|digits| (digits, true)).or_else(|| first.strip_prefix("int").map(|digits| (digits, false))) {
            Some((digits, unsigned)) if width(digits).is_some_and(|bits| matches!(bits, 8 | 16 | 32 | 64 | 128 | 256)) => {
                return integer(width(digits).unwrap_or(64), unsigned);
            }
            _ => {}
        }
    }
    match first {
        "tinyint" if numbers == [1] && matches!(kind, ConnectionKind::Mysql | ConnectionKind::Mariadb) => Family::Boolean,
        "bool" | "boolean" => Family::Boolean,
        "tinyint" | "int1" => integer(8, unsigned),
        "smallint" | "int2" | "smallserial" | "serial2" | "year" => integer(16, unsigned),
        "mediumint" | "int3" => integer(24, unsigned),
        "int" | "integer" | "int4" | "serial" | "serial4" => integer(32, unsigned),
        "bigint" | "int8" | "int64" | "bigserial" | "serial8" => integer(64, unsigned),
        "numeric" | "decimal" | "dec" | "fixed" | "number" => Family::Decimal { precision, scale: numbers.get(1).copied() },
        "decimal32" => Family::Decimal { precision: Some(9), scale: precision },
        "decimal64" => Family::Decimal { precision: Some(18), scale: precision },
        "decimal128" => Family::Decimal { precision: Some(38), scale: precision },
        "decimal256" => Family::Decimal { precision: Some(76), scale: precision },
        "real" | "float4" | "float32" => Family::Float { double: false },
        "float" if matches!(kind, ConnectionKind::Postgres) && precision.is_some_and(|bits| bits > 24) => Family::Float { double: true },
        "float" if matches!(kind, ConnectionKind::Mysql | ConnectionKind::Mariadb | ConnectionKind::Postgres) => Family::Float { double: false },
        "float" | "double" | "float8" | "float64" => Family::Float { double: true },
        "date" | "date32" => Family::Date,
        "time" | "timetz" => Family::Time,
        "timestamptz" => Family::Timestamp { zoned: true },
        "timestamp" | "datetime" | "datetime2" | "smalldatetime" | "datetime64" => {
            // `with time zone` (PostgreSQL), a zone argument (ClickHouse `DateTime64(6, 'UTC')`). A MySQL `timestamp` and a
            // ClickHouse `DateTime` are instants (stored as UTC, shown in the session's zone), whatever their spelling.
            let instant = (kind == ConnectionKind::Clickhouse && first.starts_with("datetime")) || (first == "timestamp" && matches!(kind, ConnectionKind::Mysql | ConnectionKind::Mariadb));
            let zoned = instant || (has("with") && has("zone") && !has("without")) || (first.starts_with("datetime") && text.contains('\''));
            Family::Timestamp { zoned }
        }
        "char" | "character" | "bpchar" | "nchar" | "fixedstring" if !has("varying") => Family::Text { length: precision, fixed: true },
        "varchar" | "character" | "char" | "nvarchar" | "nchar" | "string" | "varchar2" | "citext" => Family::Text { length: precision, fixed: false },
        "text" | "tinytext" | "mediumtext" | "longtext" | "clob" | "name" => Family::Text { length: None, fixed: false },
        "bytea" | "blob" | "tinyblob" | "mediumblob" | "longblob" | "binary" | "varbinary" => Family::Binary,
        "json" | "jsonb" | "object" => Family::Json,
        "uuid" => Family::Uuid,
        _ => Family::Other,
    }
}

/// The family of a type declared in SQLite: its own affinity rules, on the declared name.
fn sqlite_family(data_type: &str) -> Family {
    let (words, numbers, _) = parts(data_type);
    let name = words.join(" ");
    let has = |needle: &str| name.contains(needle);
    if name == "boolean" || name == "bool" {
        Family::Boolean
    } else if name == "date" {
        Family::Date
    } else if has("datetime") || has("timestamp") {
        Family::Timestamp { zoned: false }
    } else if has("int") {
        Family::Integer { bits: 64, unsigned: false }
    } else if has("char") || has("clob") || has("text") {
        Family::Text { length: numbers.first().copied(), fixed: false }
    } else if has("blob") {
        Family::Binary
    } else if has("real") || has("floa") || has("doub") {
        Family::Float { double: true }
    } else if has("decimal") || has("numeric") {
        Family::Decimal { precision: numbers.first().copied(), scale: numbers.get(1).copied() }
    } else if has("json") {
        Family::Json
    } else {
        Family::Other
    }
}

fn read(kind: ConnectionKind, data_type: &str) -> Family {
    if kind == ConnectionKind::Sqlite {
        sqlite_family(data_type)
    } else {
        family_of(kind, data_type)
    }
}

/// `family` in the words of the server `kind`.
pub(in crate::app) fn render(family: Family, kind: ConnectionKind) -> String {
    use ConnectionKind::{Clickhouse, Mariadb, Mysql, Postgres, Sqlite};
    match (kind, family) {
        // An integer past 64 bits and a decimal past 15 digits would be rounded by SQLite's NUMERIC (a double): text keeps them.
        (Sqlite, Family::Integer { bits, unsigned: true }) if bits >= 64 => "TEXT".into(),
        (Sqlite, Family::Integer { bits, .. }) if bits > 64 => "TEXT".into(),
        (Sqlite, Family::Integer { .. }) => "INTEGER".into(),
        (Sqlite, Family::Decimal { precision: Some(precision), .. }) if precision <= 15 => "NUMERIC".into(),
        (Sqlite, Family::Decimal { .. }) => "TEXT".into(),
        (Sqlite, Family::Float { .. }) => "REAL".into(),
        (Sqlite, Family::Boolean) => "BOOLEAN".into(),
        (Sqlite, Family::Date) => "DATE".into(),
        (Sqlite, Family::Timestamp { .. }) => "DATETIME".into(),
        (Sqlite, Family::Binary) => "BLOB".into(),
        (Sqlite, _) => "TEXT".into(),

        (Postgres, Family::Integer { bits: 8 | 16, unsigned: false }) | (Postgres, Family::Integer { bits: 8, unsigned: true }) => "smallint".into(),
        (Postgres, Family::Integer { bits: 16 | 24, unsigned: true }) | (Postgres, Family::Integer { bits: 24, unsigned: false }) => "integer".into(),
        (Postgres, Family::Integer { bits: 32, unsigned: false }) => "integer".into(),
        (Postgres, Family::Integer { bits: 32, unsigned: true }) | (Postgres, Family::Integer { bits: 64, unsigned: false }) => "bigint".into(),
        (Postgres, Family::Integer { bits: 64, unsigned: true }) => "numeric(20,0)".into(),
        (Postgres, Family::Integer { bits: 128, .. }) => "numeric(39,0)".into(),
        (Postgres, Family::Integer { .. }) => "numeric(78,0)".into(),
        (Postgres, Family::Decimal { precision: Some(precision), scale: Some(scale) }) => format!("numeric({precision},{scale})"),
        (Postgres, Family::Decimal { precision: Some(precision), scale: None }) => format!("numeric({precision})"),
        (Postgres, Family::Decimal { .. }) => "numeric".into(),
        (Postgres, Family::Float { double: false }) => "real".into(),
        (Postgres, Family::Float { double: true }) => "double precision".into(),
        (Postgres, Family::Boolean) => "boolean".into(),
        (Postgres, Family::Date) => "date".into(),
        (Postgres, Family::Time) => "time".into(),
        (Postgres, Family::Timestamp { zoned: true }) => "timestamptz".into(),
        (Postgres, Family::Timestamp { zoned: false }) => "timestamp".into(),
        (Postgres, Family::Text { length: Some(length), fixed: true }) => format!("char({length})"),
        (Postgres, Family::Text { length: Some(length), fixed: false }) => format!("varchar({length})"),
        (Postgres, Family::Text { .. }) => "text".into(),
        (Postgres, Family::Binary) => "bytea".into(),
        (Postgres, Family::Json) => "jsonb".into(),
        (Postgres, Family::Uuid) => "uuid".into(),
        (Postgres, Family::Other) => "text".into(),

        (Mysql | Mariadb, Family::Integer { bits: 8, unsigned }) => if unsigned { "tinyint unsigned" } else { "tinyint" }.into(),
        (Mysql | Mariadb, Family::Integer { bits: 16, unsigned }) => if unsigned { "smallint unsigned" } else { "smallint" }.into(),
        (Mysql | Mariadb, Family::Integer { bits: 24, unsigned }) => if unsigned { "mediumint unsigned" } else { "mediumint" }.into(),
        (Mysql | Mariadb, Family::Integer { bits: 32, unsigned }) => if unsigned { "int unsigned" } else { "int" }.into(),
        (Mysql | Mariadb, Family::Integer { bits: 64, unsigned }) => if unsigned { "bigint unsigned" } else { "bigint" }.into(),
        (Mysql | Mariadb, Family::Integer { bits: 128, .. }) => "decimal(39,0)".into(),
        (Mysql | Mariadb, Family::Integer { .. }) => "decimal(65,0)".into(),
        (Mysql | Mariadb, Family::Decimal { precision: Some(precision), scale }) => {
            // MySQL stops at 65 digits, 30 of them after the point.
            let (precision, scale) = (precision.clamp(1, 65), scale.unwrap_or(0).min(30));
            format!("decimal({precision},{})", scale.min(precision))
        }
        // A numeric with no precision holds any number of digits: the most MySQL can keep.
        (Mysql | Mariadb, Family::Decimal { .. }) => "decimal(65,30)".into(),
        (Mysql | Mariadb, Family::Float { double: false }) => "float".into(),
        (Mysql | Mariadb, Family::Float { double: true }) => "double".into(),
        (Mysql | Mariadb, Family::Boolean) => "tinyint(1)".into(),
        (Mysql | Mariadb, Family::Date) => "date".into(),
        (Mysql | Mariadb, Family::Time) => "time(6)".into(),
        (Mysql | Mariadb, Family::Timestamp { .. }) => "datetime(6)".into(),
        (Mysql | Mariadb, Family::Text { length: Some(length), fixed: true }) if length <= 255 => format!("char({length})"),
        // A row holds 65,535 bytes in all (four per character): long text is a TEXT, not a column of the row.
        (Mysql | Mariadb, Family::Text { length: Some(length), .. }) if length <= 1_000 => format!("varchar({length})"),
        (Mysql | Mariadb, Family::Text { length: Some(length), .. }) if length <= 16_383 => "text".into(),
        (Mysql | Mariadb, Family::Text { .. }) => "longtext".into(),
        (Mysql | Mariadb, Family::Binary) => "longblob".into(),
        (Mysql | Mariadb, Family::Json) => "json".into(),
        (Mysql | Mariadb, Family::Uuid) => "char(36)".into(),
        (Mysql | Mariadb, Family::Other) => "longtext".into(),

        (Clickhouse, Family::Integer { bits, unsigned }) => {
            let bits = match bits {
                24 => 32,
                bits => bits,
            };
            format!("{}Int{bits}", if unsigned { "U" } else { "" })
        }
        (Clickhouse, Family::Decimal { precision: Some(precision), scale }) if precision <= 76 => format!("Decimal({precision}, {})", scale.unwrap_or(0).min(precision)),
        (Clickhouse, Family::Decimal { .. }) => "Decimal(76, 38)".into(),
        (Clickhouse, Family::Float { double: false }) => "Float32".into(),
        (Clickhouse, Family::Float { double: true }) => "Float64".into(),
        (Clickhouse, Family::Boolean) => "Bool".into(),
        (Clickhouse, Family::Date) => "Date32".into(),
        (Clickhouse, Family::Timestamp { zoned: true }) => "DateTime64(6, 'UTC')".into(),
        (Clickhouse, Family::Timestamp { zoned: false }) => "DateTime64(6)".into(),
        (Clickhouse, Family::Uuid) => "UUID".into(),
        (Clickhouse, _) => "String".into(),

        // Not SQL kinds: no table is created there.
        (_, _) => "text".into(),
    }
}

/// A type for a new column, and what the user should know about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) struct Mapped {
    pub(in crate::app) data_type: String,
    pub(in crate::app) note: Option<&'static str>,
}

/// Where the type of a source column was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum Labels {
    /// The catalog of a table: the type as declared, so a bare `datetime` is a DATETIME with no fraction.
    Catalog,
    /// A page of a query: the driver's label, which can leave out a size or name an Arrow type.
    Result,
}

/// The type of the table a transfer creates on a `to` server for a column the `from` server's catalog calls `data_type`.
#[cfg(test)]
pub(in crate::app) fn map_type(from: ConnectionKind, data_type: &str, to: ConnectionKind) -> String {
    map_type_noted(from, data_type, to, Labels::Catalog).data_type
}

/// [`map_type`], with a note where the new type cannot hold everything the old one could.
pub(in crate::app) fn map_type_noted(from: ConnectionKind, data_type: &str, to: ConnectionKind, labels: Labels) -> Mapped {
    use ConnectionKind::{Clickhouse, Mariadb, Mysql, Postgres};
    // A result column of a query is labelled by the driver with a bare type (`varchar`, `char`, `decimal`, `enum`, `datetime`): as
    // written it would make another column (CHAR(1), DECIMAL(10,0), a DATETIME with no fraction, an ENUM with no values, or an
    // error). What the label cannot say is taken from the family, which holds every value. A catalog says the size where it has
    // one: its bare `datetime` is a DATETIME(0) and is kept.
    let label = data_type.trim().to_ascii_lowercase();
    let result = labels == Labels::Result;
    let mysql_label = result && matches!(from, Mysql | Mariadb);
    if mysql_label && same_family(from, to) && matches!(label.as_str(), "datetime" | "timestamp" | "time") {
        return Mapped { data_type: format!("{label}(6)"), note: None };
    }
    // A PostgreSQL result column is labelled by its Arrow type (`int64`, `float64`, …), which no PostgreSQL table has.
    let arrow_label = result && from == Postgres && matches!(label.as_str(), "int64" | "float32" | "float64" | "binary");
    let bare = arrow_label || matches!(label.as_str(), "varchar" | "nvarchar" | "varbinary") || (mysql_label && matches!(label.as_str(), "char" | "binary" | "decimal" | "numeric" | "enum" | "set" | "null"));
    if same_family(from, to) && !label.is_empty() && !bare {
        return Mapped { data_type: data_type.trim().to_owned(), note: None };
    }
    let family = read(from, data_type);
    let note = match (from, to, family) {
        (Mysql | Mariadb, _, Family::Boolean) => Some("A tinyint(1) becomes a boolean: a value other than 0 and 1 is refused."),
        (_, Mysql | Mariadb, Family::Decimal { precision: None, .. }) => Some("No precision: DECIMAL(65,30) holds 35 digits before the point and 30 after; more decimals are rounded, a larger number is refused. Use text to keep every digit."),
        (_, Mysql | Mariadb, Family::Decimal { precision: Some(precision), scale }) if precision > 65 || scale.is_some_and(|scale| scale > 30) => {
            Some("MySQL holds 65 digits, 30 of them decimals: more is rounded.")
        }
        (_, Clickhouse, Family::Decimal { precision: None, .. }) => Some("No precision: up to 38 decimals are kept, the rest is rounded. Use String to keep every digit."),
        (_, Clickhouse, Family::Date | Family::Timestamp { .. }) => Some("A date or time before 1900 cannot be held (ClickHouse Date32 and DateTime64)."),
        (_, Mysql | Mariadb, Family::Timestamp { zoned: true }) => Some("The instant is stored as its UTC time of day."),
        (_, Mysql | Mariadb, Family::Text { length: Some(length), .. }) if length > 1_000 => Some("Long text is a TEXT column: the row limit of MySQL is 65,535 bytes."),
        _ => None,
    };
    Mapped { data_type: render(family, to), note }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ConnectionKind::{Clickhouse, Mariadb, Mysql, Postgres, Sqlite};

    fn mapped(from: ConnectionKind, data_type: &str, to: ConnectionKind) -> String {
        map_type(from, data_type, to)
    }

    /// The type made for a query's column the driver labels `data_type`.
    fn labelled(from: ConnectionKind, data_type: &str, to: ConnectionKind) -> String {
        map_type_noted(from, data_type, to, Labels::Result).data_type
    }

    #[test]
    fn a_query_column_that_says_varchar_with_no_length_becomes_text_in_a_mysql_table() {
        assert_eq!(mapped(Mysql, "varchar", Mysql), "longtext");
        assert_eq!(mapped(Mysql, "varchar", Mariadb), "longtext");
        assert_eq!(mapped(Mysql, "varbinary", Mysql), "longblob");
        assert_eq!(mapped(Mysql, "varchar(20)", Mysql), "varchar(20)", "a type with its length is kept as it is");
        // The other labels a MySQL result carries with no size would be CHAR(1), DECIMAL(10,0), BINARY(1), a DATETIME with no
        // fraction, or no type at all (`enum`, `set`).
        assert_eq!(labelled(Mysql, "char", Mysql), "longtext");
        assert_eq!(labelled(Mysql, "binary", Mariadb), "longblob");
        assert_eq!(labelled(Mysql, "decimal", Mysql), "decimal(65,30)");
        assert_eq!(labelled(Mysql, "enum", Mysql), "longtext");
        assert_eq!(labelled(Mysql, "set", Mariadb), "longtext");
        assert_eq!(labelled(Mysql, "null", Mysql), "longtext");
        assert_eq!(labelled(Mysql, "datetime", Mysql), "datetime(6)");
        assert_eq!(labelled(Mariadb, "timestamp", Mysql), "timestamp(6)");
        assert_eq!(labelled(Mysql, "time", Mysql), "time(6)");
        // A type with its size, and the types of a dialect that spells them bare, are kept.
        assert_eq!(mapped(Mysql, "char(3)", Mysql), "char(3)");
        assert_eq!(mapped(Mysql, "decimal(12,4)", Mysql), "decimal(12,4)");
        assert_eq!(mapped(Mysql, "datetime(3)", Mysql), "datetime(3)");
        assert_eq!(mapped(Sqlite, "DECIMAL", Sqlite), "DECIMAL");
        assert_eq!(mapped(Postgres, "numeric", Postgres), "numeric");
        // A PostgreSQL query labels its columns by their Arrow type, which is not a type of PostgreSQL.
        for (label, pg, mysql) in [("int64", "bigint", "bigint"), ("float64", "double precision", "double"), ("float32", "real", "float"), ("binary", "bytea", "longblob"), ("boolean", "boolean", "tinyint(1)"), ("text", "text", "longtext")] {
            assert_eq!((labelled(Postgres, label, Postgres).as_str(), labelled(Postgres, label, Mysql).as_str()), (pg, mysql), "{label}");
        }
        assert_eq!(labelled(Postgres, "bigint", Postgres), "bigint", "a table's own type is kept");
        // A table's catalog gives the size where it has one: its bare types are what they say.
        assert_eq!(mapped(Mysql, "datetime", Mysql), "datetime");
        assert_eq!(mapped(Mariadb, "timestamp", Mariadb), "timestamp");
        assert_eq!(mapped(Mysql, "time", Mysql), "time");
        assert_eq!(mapped(Postgres, "int8", Postgres), "int8");
    }

    #[test]
    fn postgresql_types_become_the_closest_type_of_each_other_dialect() {
        let types = ["integer", "bigint", "smallint", "numeric(12,2)", "double precision", "boolean", "text", "character varying(120)", "character(3)", "date", "timestamp without time zone", "timestamp with time zone", "bytea", "jsonb", "uuid"];
        let expected = |to: ConnectionKind| -> Vec<&'static str> {
            match to {
                Mysql => vec!["int", "bigint", "smallint", "decimal(12,2)", "double", "tinyint(1)", "longtext", "varchar(120)", "char(3)", "date", "datetime(6)", "datetime(6)", "longblob", "json", "char(36)"],
                Sqlite => vec!["INTEGER", "INTEGER", "INTEGER", "NUMERIC", "REAL", "BOOLEAN", "TEXT", "TEXT", "TEXT", "DATE", "DATETIME", "DATETIME", "BLOB", "TEXT", "TEXT"],
                Clickhouse => vec!["Int32", "Int64", "Int16", "Decimal(12, 2)", "Float64", "Bool", "String", "String", "String", "Date32", "DateTime64(6)", "DateTime64(6, 'UTC')", "String", "String", "UUID"],
                _ => unreachable!(),
            }
        };
        for to in [Mysql, Sqlite, Clickhouse] {
            let got: Vec<String> = types.iter().map(|data_type| mapped(Postgres, data_type, to)).collect();
            assert_eq!(got, expected(to), "to {to:?}");
        }
    }

    #[test]
    fn mysql_types_keep_their_size_and_sign_where_the_target_has_one() {
        assert_eq!(mapped(Mysql, "bigint unsigned", Postgres), "numeric(20,0)");
        assert_eq!(mapped(Mysql, "int unsigned", Postgres), "bigint");
        assert_eq!(mapped(Mysql, "tinyint", Postgres), "smallint");
        assert_eq!(mapped(Mysql, "tinyint(1)", Postgres), "boolean", "the MySQL spelling of a flag");
        assert_eq!(mapped(Mysql, "mediumint", Postgres), "integer");
        assert_eq!(mapped(Mysql, "decimal(65,30)", Postgres), "numeric(65,30)");
        assert_eq!(mapped(Mysql, "float", Postgres), "real");
        assert_eq!(mapped(Mysql, "datetime(3)", Postgres), "timestamp");
        assert_eq!(mapped(Mysql, "longtext", Postgres), "text");
        assert_eq!(mapped(Mysql, "varchar(255)", Postgres), "varchar(255)");
        assert_eq!(mapped(Mysql, "longblob", Postgres), "bytea");
        assert_eq!(mapped(Mysql, "json", Postgres), "jsonb");
        assert_eq!(mapped(Mysql, "enum('a','b')", Postgres), "text");
        assert_eq!(mapped(Mysql, "bigint unsigned", Clickhouse), "UInt64");
        assert_eq!(mapped(Mysql, "bigint unsigned", Sqlite), "TEXT", "an unsigned 64-bit value can exceed an SQLite integer, and NUMERIC would round it");
        assert_eq!(mapped(Mysql, "int unsigned", Sqlite), "INTEGER");
    }

    #[test]
    fn a_decimal_is_clamped_to_what_the_target_accepts() {
        assert_eq!(mapped(Postgres, "numeric(1000,500)", Mysql), "decimal(65,30)");
        assert_eq!(mapped(Postgres, "numeric", Mysql), "decimal(65,30)");
        assert_eq!(mapped(Postgres, "numeric", Clickhouse), "Decimal(76, 38)");
        assert_eq!(mapped(Postgres, "numeric(5)", Mysql), "decimal(5,0)");
        assert_eq!(mapped(Clickhouse, "Decimal(18, 4)", Postgres), "numeric(18,4)");
        assert_eq!(mapped(Clickhouse, "Decimal64(4)", Postgres), "numeric(18,4)");
    }

    #[test]
    fn clickhouse_wrappers_and_sizes_are_read() {
        assert_eq!(mapped(Clickhouse, "Nullable(Int32)", Postgres), "integer");
        assert_eq!(mapped(Clickhouse, "LowCardinality(Nullable(String))", Postgres), "text");
        assert_eq!(mapped(Clickhouse, "UInt64", Postgres), "numeric(20,0)");
        assert_eq!(mapped(Clickhouse, "Int8", Mysql), "tinyint");
        assert_eq!(mapped(Clickhouse, "Float32", Mysql), "float");
        assert_eq!(mapped(Clickhouse, "DateTime64(3, 'UTC')", Postgres), "timestamptz");
        assert_eq!(mapped(Clickhouse, "DateTime", Postgres), "timestamptz", "a ClickHouse DateTime is an instant");
        assert_eq!(mapped(Clickhouse, "Array(Int32)", Postgres), "text");
        assert_eq!(mapped(Clickhouse, "UUID", Mysql), "char(36)");
        assert_eq!(mapped(Clickhouse, "Int256", Postgres), "numeric(78,0)");
    }

    #[test]
    fn sqlite_declared_types_are_read_by_the_affinity_rules() {
        assert_eq!(mapped(Sqlite, "INTEGER", Postgres), "bigint");
        assert_eq!(mapped(Sqlite, "VARCHAR(40)", Postgres), "varchar(40)");
        assert_eq!(mapped(Sqlite, "DATETIME", Postgres), "timestamp");
        assert_eq!(mapped(Sqlite, "BOOLEAN", Mysql), "tinyint(1)");
        assert_eq!(mapped(Sqlite, "REAL", Postgres), "double precision");
        assert_eq!(mapped(Sqlite, "DECIMAL(10,2)", Postgres), "numeric(10,2)");
        assert_eq!(mapped(Sqlite, "BLOB", Postgres), "bytea");
        assert_eq!(mapped(Sqlite, "", Postgres), "text", "a column with no declared type");
        assert_eq!(mapped(Sqlite, "NVARCHAR(10)", Mysql), "varchar(10)");
    }

    #[test]
    fn the_same_dialect_keeps_the_type_as_the_server_reports_it() {
        assert_eq!(mapped(Postgres, "character varying(10)[]", Postgres), "character varying(10)[]");
        assert_eq!(mapped(Mysql, "enum('a','b')", Mariadb), "enum('a','b')");
        assert_eq!(mapped(Mariadb, "bigint unsigned", Mysql), "bigint unsigned");
        assert_eq!(mapped(Sqlite, "WHATEVER", Sqlite), "WHATEVER");
        assert_eq!(mapped(Clickhouse, "Nullable(Int32)", Clickhouse), "Nullable(Int32)");
    }

    #[test]
    fn an_instant_stays_an_instant_and_a_wall_clock_a_wall_clock() {
        assert_eq!(mapped(Mysql, "timestamp", Postgres), "timestamptz");
        assert_eq!(mapped(Mysql, "datetime(6)", Postgres), "timestamp");
        assert_eq!(mapped(Mysql, "timestamp", Clickhouse), "DateTime64(6, 'UTC')");
        assert_eq!(mapped(Clickhouse, "DateTime64(3)", Mysql), "datetime(6)");
        assert_eq!(mapped(Clickhouse, "DateTime", Postgres), "timestamptz");
        assert_eq!(mapped(Postgres, "timestamp without time zone", Clickhouse), "DateTime64(6)");
    }

    #[test]
    fn long_text_is_a_text_column_and_exact_numbers_are_not_rounded_by_sqlite() {
        assert_eq!(mapped(Postgres, "character varying(1000)", Mysql), "varchar(1000)");
        assert_eq!(mapped(Postgres, "character varying(16000)", Mysql), "text");
        assert_eq!(mapped(Postgres, "character varying(20000)", Mysql), "longtext");
        assert_eq!(mapped(Postgres, "numeric(12,2)", Sqlite), "NUMERIC");
        assert_eq!(mapped(Postgres, "numeric(30,10)", Sqlite), "TEXT");
        assert_eq!(mapped(Postgres, "numeric", Sqlite), "TEXT");
        assert_eq!(mapped(Clickhouse, "UInt256", Sqlite), "TEXT");
    }

    #[test]
    fn a_type_that_cannot_hold_everything_comes_with_a_note() {
        let note = |from, data_type: &str, to| map_type_noted(from, data_type, to, Labels::Catalog).note;
        assert!(note(Postgres, "numeric", Mysql).is_some_and(|note| note.contains("35 digits before the point and 30 after")));
        assert!(note(Postgres, "numeric(70,40)", Mysql).is_some_and(|note| note.contains("65 digits")));
        assert!(note(Postgres, "numeric", Clickhouse).is_some_and(|note| note.contains("38 decimals")));
        assert!(note(Mysql, "tinyint(1)", Postgres).is_some_and(|note| note.contains("other than 0 and 1")));
        assert!(note(Postgres, "date", Clickhouse).is_some_and(|note| note.contains("1900")));
        assert!(note(Postgres, "character varying(5000)", Mysql).is_some_and(|note| note.contains("TEXT")));
        assert!(note(Postgres, "timestamptz", Mysql).is_some_and(|note| note.contains("UTC")));
        assert_eq!(note(Postgres, "integer", Mysql), None);
        assert_eq!(note(Mysql, "tinyint(1)", Mariadb), None, "the same dialect keeps its own type");
    }

    #[test]
    fn what_has_no_counterpart_travels_as_text() {
        for data_type in ["integer[]", "interval", "tsvector", "mood", "point"] {
            assert_eq!(mapped(Postgres, data_type, Mysql), "longtext", "{data_type}");
            assert_eq!(mapped(Postgres, data_type, Sqlite), "TEXT", "{data_type}");
        }
        assert_eq!(mapped(Postgres, "time without time zone", Clickhouse), "String");
        assert_eq!(mapped(Postgres, "time without time zone", Mysql), "time(6)");
    }

    #[test]
    fn every_rendered_type_reads_back_to_the_family_it_came_from() {
        let families = [
            Family::Integer { bits: 32, unsigned: false },
            Family::Integer { bits: 64, unsigned: false },
            Family::Float { double: true },
            Family::Boolean,
            Family::Date,
            Family::Timestamp { zoned: false },
            Family::Binary,
            Family::Json,
            Family::Uuid,
        ];
        for kind in [Postgres, Mysql, Clickhouse] {
            for family in families {
                let text = render(family, kind);
                let back = family_of(kind, &text);
                // JSON and UUID are text in MySQL, binary is a blob in all: only the ones the dialect has are checked.
                if matches!((kind, family), (Mysql, Family::Uuid) | (Clickhouse, Family::Json | Family::Binary | Family::Timestamp { zoned: false })) {
                    continue;
                }
                assert_eq!(back, family, "{kind:?}: {text}");
            }
        }
    }
}
