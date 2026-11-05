//! SQL row mutations (`insert` / `update` / `delete` / `batch`), per
//! `engine_driver_api::sql`.
//!
//! Every request runs inside a `SAVEPOINT`, which works both in autocommit mode
//! (the outermost savepoint is the transaction: `RELEASE` commits) and inside a
//! transaction the user opened with `BEGIN` (the mutation joins it; `RELEASE`
//! keeps it pending, and a failure rolls back to the savepoint only, so the
//! user's transaction is never committed or rolled back by the driver). All
//! values are bound parameters; keys compare with `IS`, which is null-safe.
//!
//! # Storage classes
//!
//! SQLite stores a class per cell, so a display string is not enough to write a
//! value back:
//!
//! * An update binds the new text in the storage class the cell has now
//!   (`typeof()` of the row, read inside the same savepoint) when the text is a
//!   valid value of that class: an INTEGER stays INTEGER, a REAL stays REAL
//!   (`inf`, `-inf`, `Infinity` ... spelled case-insensitively),
//!   TEXT stays TEXT. Text that is not a valid number for a numeric cell
//!   is stored as the text it is. A cell that is NULL, and an insert, follow
//!   the column's declared type by SQLite's affinity rules. `BOOLEAN` columns
//!   also take `true` / `false`.
//! * A BLOB cell, and a column declared BLOB when the cell is NULL or on insert,
//!   take `0x` + hex digits (either case; `0x` is the empty blob) and bind the
//!   real bytes; any other text is refused with `mutation.invalid_value`. A TEXT
//!   cell of a BLOB column stays text: `0x41` is just text there.
//! * `NaN` cannot be stored (SQLite turns it into NULL): refused.
//! * A key given as a string matches the cell in whichever class the text can be
//!   read as (text, `0x` hex as a blob, a canonical integer, a canonical real),
//!   so a mixed column's keys and a BLOB key work; a key that matches several
//!   rows is refused as ever.

use engine_driver_kit::api::sql::{codes, SqlBatch, SqlRowChange, SqlRowOp};
use engine_driver_kit::api::MutationRequest;
use engine_driver_kit::BackendError;
use rusqlite::types::Value;
use rusqlite::{params_from_iter, Connection};
use serde_json::Map;
use std::borrow::Cow;

/// Failure of one change: a contract error or a SQLite one.
#[derive(Debug)]
pub(crate) enum Failure {
    Contract { code: &'static str, message: String },
    Sqlite(rusqlite::Error),
}

impl From<rusqlite::Error> for Failure {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

fn contract(code: &'static str, message: impl Into<String>) -> Failure {
    Failure::Contract {
        code,
        message: message.into(),
    }
}

/// A parsed request: the table and its changes (one for `insert`/`update`/
/// `delete`, the whole list for `batch`).
pub(crate) struct Plan {
    pub(crate) table: String,
    pub(crate) changes: Vec<SqlRowChange>,
}

impl Plan {
    pub(crate) fn from_request(request: &MutationRequest) -> Result<Self, BackendError> {
        let bad = |message: String| BackendError::with_code(codes::MUTATION_BAD_REQUEST, message);
        let changes = match request.kind.as_str() {
            kind @ ("insert" | "update" | "delete") => {
                let change: SqlRowChange = serde_json::from_value(request.payload.clone())
                    .map_err(|error| bad(format!("payload is not a SqlRowChange: {error}")))?;
                if change.op.kind() != kind {
                    return Err(bad(format!(
                        "payload op `{}` does not match kind `{kind}`",
                        change.op.kind()
                    )));
                }
                vec![change]
            }
            "batch" => {
                let batch: SqlBatch = serde_json::from_value(request.payload.clone())
                    .map_err(|error| bad(format!("payload is not a SqlBatch: {error}")))?;
                if batch.changes.is_empty() {
                    return Err(bad("a batch needs at least one change".into()));
                }
                batch.changes
            }
            other => {
                return Err(BackendError::with_code(
                    codes::MUTATION_UNSUPPORTED,
                    format!("SQLite does not support mutation `{other}`"),
                ))
            }
        };
        if let Some(database) = request.database.as_deref().filter(|d| !d.is_empty()) {
            if !database.eq_ignore_ascii_case(crate::catalog::MAIN) {
                return Err(bad(format!(
                    "SQLite exposes only the `main` database, not `{database}`"
                )));
            }
        }
        let table = request
            .object
            .clone()
            .filter(|table| !table.is_empty())
            .ok_or_else(|| bad("a mutation needs the table in `object`".into()))?;
        Ok(Self { table, changes })
    }
}

/// `"name"` with embedded quotes doubled.
pub(crate) fn quote_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The SQL parameter for a JSON value: `null` is NULL, booleans are 0/1,
/// numbers keep integer-ness, arrays and objects become JSON text.
pub(crate) fn bind_value(value: &serde_json::Value) -> Value {
    match value {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(flag) => Value::Integer(i64::from(*flag)),
        serde_json::Value::Number(number) => number.as_i64().map_or_else(
            || Value::Real(number.as_f64().unwrap_or(f64::NAN)),
            Value::Integer,
        ),
        serde_json::Value::String(text) => Value::Text(text.clone()),
        other => Value::Text(other.to_string()),
    }
}

fn assignments(values: &Map<String, serde_json::Value>, separator: &str, operator: &str) -> String {
    values
        .keys()
        .map(|column| format!("{} {operator} ?", quote_identifier(column)))
        .collect::<Vec<_>>()
        .join(separator)
}

/// A storage class, as `typeof()` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Null,
    Integer,
    Real,
    Text,
    Blob,
}

impl Class {
    fn from_typeof(name: &str) -> Self {
        match name {
            "integer" => Self::Integer,
            "real" => Self::Real,
            "text" => Self::Text,
            "blob" => Self::Blob,
            _ => Self::Null,
        }
    }
}

/// How a text is read when it is written to a cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    Integer,
    Real,
    /// NUMERIC affinity: an integer, else a real, else text.
    Numeric,
    Text,
    /// `0x` hex bytes only.
    Blob,
    /// No declared type (BLOB affinity with no type name): the value as given.
    Any,
}

/// One column of the table: its declared type, which decides what an insert or a
/// NULL cell takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Column {
    name: String,
    declared: String,
}

impl Column {
    /// SQLite's affinity rules (https://sqlite.org/datatype3.html#determination_of_column_affinity).
    fn declared_target(&self) -> Target {
        let declared = self.declared.to_ascii_uppercase();
        if declared.contains("INT") {
            Target::Integer
        } else if declared.contains("CHAR")
            || declared.contains("CLOB")
            || declared.contains("TEXT")
        {
            Target::Text
        } else if declared.contains("BLOB") {
            Target::Blob
        } else if declared.is_empty() {
            Target::Any
        } else if declared.contains("REAL")
            || declared.contains("FLOA")
            || declared.contains("DOUB")
        {
            Target::Real
        } else {
            Target::Numeric
        }
    }

    fn is_boolean(&self) -> bool {
        let declared = self.declared.trim();
        declared.eq_ignore_ascii_case("BOOLEAN") || declared.eq_ignore_ascii_case("BOOL")
    }

    /// The class text is read as for a cell currently of `current`.
    fn target(&self, current: Class) -> Target {
        match current {
            Class::Integer => Target::Integer,
            Class::Real => Target::Real,
            Class::Text => Target::Text,
            Class::Blob => Target::Blob,
            Class::Null => self.declared_target(),
        }
    }
}

/// The declared columns of the table.
#[derive(Debug, Default)]
pub(crate) struct TableInfo {
    columns: Vec<Column>,
}

impl TableInfo {
    pub(crate) fn load(conn: &Connection, table: &str) -> rusqlite::Result<Self> {
        let mut statement = conn.prepare("SELECT name, type FROM pragma_table_info(?)")?;
        let columns = statement
            .query_map([table], |row| {
                Ok(Column {
                    name: row.get(0)?,
                    declared: row.get(1)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Self { columns })
    }

    /// Column names are case-insensitive in SQLite.
    fn column(&self, name: &str) -> Option<&Column> {
        self.columns
            .iter()
            .find(|column| column.name.eq_ignore_ascii_case(name))
    }
}

fn invalid_value(column: &str, message: impl std::fmt::Display) -> Failure {
    contract(
        codes::MUTATION_INVALID_VALUE,
        format!("column `{column}`: {message}"),
    )
}

/// `0x` followed by an even number of hex digits (either case) as bytes.
fn parse_hex_bytes(text: &str) -> Option<Vec<u8>> {
    let digits = text.trim().strip_prefix("0x")?;
    if digits.len() % 2 != 0 {
        return None;
    }
    digits
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = char::from(pair[0]).to_digit(16)?;
            let low = char::from(pair[1]).to_digit(16)?;
            u8::try_from(high << 4 | low).ok()
        })
        .collect()
}

/// A real as the editor reads it: `inf`, `+inf`, `-inf`,
/// `infinity`, `nan` (any case) and every number Rust parses.
fn parse_real(text: &str) -> Option<f64> {
    let text = text.trim();
    match text.to_ascii_lowercase().as_str() {
        "inf" | "+inf" | "infinity" | "+infinity" => Some(f64::INFINITY),
        "-inf" | "-infinity" => Some(f64::NEG_INFINITY),
        "nan" => Some(f64::NAN),
        _ => text.parse().ok(),
    }
}

/// The SQL parameter for `value`, written to `column` whose cell is read as `target`.
fn bind_cell(column: &Column, target: Target, value: &serde_json::Value) -> Result<Value, Failure> {
    let name = column.name.as_str();
    match value {
        serde_json::Value::Number(number) => match target {
            Target::Text => Ok(Value::Text(number.to_string())),
            Target::Blob => Err(invalid_value(
                name,
                "a BLOB column takes `0x` followed by an even number of hex digits",
            )),
            Target::Real => Ok(Value::Real(number.as_f64().unwrap_or(f64::NAN))),
            _ => Ok(bind_value(value)),
        },
        serde_json::Value::String(text) => bind_text(column, target, text),
        other => Ok(bind_value(other)),
    }
}

fn bind_text(column: &Column, target: Target, text: &str) -> Result<Value, Failure> {
    let name = column.name.as_str();
    match target {
        Target::Text | Target::Any => Ok(Value::Text(text.to_owned())),
        Target::Blob => parse_hex_bytes(text).map(Value::Blob).ok_or_else(|| {
            invalid_value(
                name,
                "a BLOB takes `0x` followed by an even number of hex digits",
            )
        }),
        Target::Integer | Target::Numeric => {
            if column.is_boolean() {
                match text.trim().to_ascii_lowercase().as_str() {
                    "true" => return Ok(Value::Integer(1)),
                    "false" => return Ok(Value::Integer(0)),
                    _ => {}
                }
            }
            if let Ok(integer) = text.trim().parse::<i64>() {
                return Ok(Value::Integer(integer));
            }
            // A number stays a number: `1.5` in an INTEGER cell is a REAL, a whole
            // `5.0` an INTEGER; text that is no number is the text it is.
            Ok(match parse_real(text) {
                Some(real) if real.is_nan() => Value::Text(text.to_owned()),
                Some(real)
                    if target == Target::Integer && real.fract() == 0.0 && real.abs() < 9.2e18 =>
                {
                    Value::Integer(real as i64)
                }
                Some(real) => Value::Real(real),
                None => Value::Text(text.to_owned()),
            })
        }
        Target::Real => match parse_real(text) {
            Some(real) if real.is_nan() => Err(invalid_value(
                name,
                "NaN cannot be stored in SQLite (it becomes NULL); write NULL if that is meant",
            )),
            Some(real) => Ok(Value::Real(real)),
            None => Ok(Value::Text(text.to_owned())),
        },
    }
}

/// `"c" IS ?`, or `("c" IS ? OR "c" IS ? ...)` for a key text that can be read in
/// several classes; appends the SQL and the bound values.
fn push_key(
    sql: &mut String,
    bound: &mut Vec<Value>,
    column: &str,
    value: &serde_json::Value,
    info: &TableInfo,
) {
    let quoted = quote_identifier(column);
    let candidates = match value {
        serde_json::Value::String(text) => key_candidates(text, info.column(column)),
        other => vec![bind_value(other)],
    };
    let several = candidates.len() > 1;
    if several {
        sql.push('(');
    }
    for (index, candidate) in candidates.into_iter().enumerate() {
        if index > 0 {
            sql.push_str(" OR ");
        }
        sql.push_str(&quoted);
        sql.push_str(" IS ?");
        bound.push(candidate);
    }
    if several {
        sql.push(')');
    }
}

/// The values a key text may stand for. The text itself always; a blob for `0x`
/// hex, an integer or a real for the canonical spelling of one (`01` is only
/// ever text), so a key in a mixed or BLOB column matches the cell it came from.
fn key_candidates(text: &str, column: Option<&Column>) -> Vec<Value> {
    let mut candidates = vec![Value::Text(text.to_owned())];
    if let Some(bytes) = parse_hex_bytes(text) {
        candidates.push(Value::Blob(bytes));
    }
    if let Ok(integer) = text.parse::<i64>() {
        if integer.to_string() == text {
            candidates.push(Value::Integer(integer));
        }
    } else if let Some(real) = parse_real(text) {
        if !real.is_nan() && (real.is_infinite() || real.to_string() == text) {
            candidates.push(Value::Real(real));
        }
    }
    if column.is_some_and(Column::is_boolean) {
        match text.trim().to_ascii_lowercase().as_str() {
            "true" => candidates.push(Value::Integer(1)),
            "false" => candidates.push(Value::Integer(0)),
            _ => {}
        }
    }
    candidates
}

/// The `WHERE` of a key and its bound values.
fn key_clause(key: &Map<String, serde_json::Value>, info: &TableInfo) -> (String, Vec<Value>) {
    let mut sql = String::new();
    let mut bound = Vec::new();
    for (index, (column, value)) in key.iter().enumerate() {
        if index > 0 {
            sql.push_str(" AND ");
        }
        push_key(&mut sql, &mut bound, column, value, info);
    }
    (sql, bound)
}

/// The storage class of each updated column in the row the key names (all
/// `Null` when no row matches: the update then reports it).
fn current_classes(
    conn: &Connection,
    table_sql: &str,
    values: &Map<String, serde_json::Value>,
    key_sql: &str,
    key_bound: &[Value],
) -> rusqlite::Result<Vec<Class>> {
    let projection = values
        .keys()
        .map(|column| format!("typeof({})", quote_identifier(column)))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!("SELECT {projection} FROM {table_sql} WHERE {key_sql} LIMIT 1");
    let mut statement = conn.prepare(&sql)?;
    let mut rows = statement.query(params_from_iter(key_bound.iter()))?;
    Ok(match rows.next()? {
        Some(row) => (0..values.len())
            .map(|index| {
                row.get::<_, String>(index)
                    .map_or(Class::Null, |name| Class::from_typeof(&name))
            })
            .collect(),
        None => vec![Class::Null; values.len()],
    })
}

/// Runs one change; returns the rows it changed (always 1 on success).
#[cfg(test)]
pub(crate) fn apply(conn: &Connection, table: &str, change: &SqlRowChange) -> Result<u64, Failure> {
    let info = TableInfo::load(conn, table)?;
    apply_with(conn, table, &info, change)
}

fn apply_with(
    conn: &Connection,
    table: &str,
    info: &TableInfo,
    change: &SqlRowChange,
) -> Result<u64, Failure> {
    let table_sql = quote_identifier(table);
    // An unknown column name is left to SQLite's own error.
    let cell = |column: &str, class: Class, value: &serde_json::Value| {
        let column_info = info.column(column).map_or_else(
            || {
                Cow::Owned(Column {
                    name: column.to_owned(),
                    declared: String::new(),
                })
            },
            Cow::Borrowed,
        );
        bind_cell(&column_info, column_info.target(class), value)
    };
    let (sql, bound): (String, Vec<Value>) = match change.op {
        SqlRowOp::Insert => {
            if change.values.is_empty() {
                (
                    format!("INSERT INTO {table_sql} DEFAULT VALUES"),
                    Vec::new(),
                )
            } else {
                let columns = change
                    .values
                    .keys()
                    .map(|column| quote_identifier(column))
                    .collect::<Vec<_>>()
                    .join(", ");
                let marks = vec!["?"; change.values.len()].join(", ");
                let bound = change
                    .values
                    .iter()
                    .map(|(column, value)| cell(column, Class::Null, value))
                    .collect::<Result<Vec<_>, _>>()?;
                (
                    format!("INSERT INTO {table_sql} ({columns}) VALUES ({marks})"),
                    bound,
                )
            }
        }
        SqlRowOp::Update => {
            if change.key.is_empty() {
                return Err(contract(codes::MUTATION_BAD_REQUEST, "update needs a key"));
            }
            if change.values.is_empty() {
                return Err(contract(
                    codes::MUTATION_BAD_REQUEST,
                    "update needs at least one value",
                ));
            }
            let (key_sql, key_bound) = key_clause(&change.key, info);
            // The cells' current classes, in the savepoint this change runs in.
            let classes = current_classes(conn, &table_sql, &change.values, &key_sql, &key_bound)?;
            let mut bound = change
                .values
                .iter()
                .zip(classes)
                .map(|((column, value), class)| cell(column, class, value))
                .collect::<Result<Vec<_>, _>>()?;
            bound.extend(key_bound);
            (
                format!(
                    "UPDATE {table_sql} SET {} WHERE {key_sql}",
                    assignments(&change.values, ", ", "=")
                ),
                bound,
            )
        }
        SqlRowOp::Delete => {
            if change.key.is_empty() {
                return Err(contract(codes::MUTATION_BAD_REQUEST, "delete needs a key"));
            }
            let (key_sql, key_bound) = key_clause(&change.key, info);
            (
                format!("DELETE FROM {table_sql} WHERE {key_sql}"),
                key_bound,
            )
        }
    };
    let changed = conn.execute(&sql, params_from_iter(bound.iter()))?;
    match changed {
        0 => Err(contract(
            codes::MUTATION_NO_ROWS_AFFECTED,
            format!("{} on `{table}` affected no rows", change.op.kind()),
        )),
        1 => Ok(1),
        many if change.op != SqlRowOp::Insert => Err(contract(
            codes::MUTATION_IDENTITY_NOT_UNIQUE,
            format!(
                "the key matches {many} rows of `{table}`; refusing to {} more than one",
                change.op.kind()
            ),
        )),
        _ => Ok(1),
    }
}

/// Applies every change in order; the first failure aborts. The failing
/// change's index is prefixed to batch errors.
pub(crate) fn apply_all(conn: &Connection, plan: &Plan) -> Result<u64, Failure> {
    let batch = plan.changes.len() > 1;
    let info = TableInfo::load(conn, &plan.table)?;
    let mut affected = 0;
    for (index, change) in plan.changes.iter().enumerate() {
        affected += apply_with(conn, &plan.table, &info, change).map_err(|failure| {
            if !batch {
                return failure;
            }
            match failure {
                Failure::Contract { code, message } => {
                    contract(code, format!("change {index}: {message}"))
                }
                Failure::Sqlite(error) => Failure::Sqlite(error),
            }
        })?;
    }
    Ok(affected)
}

/// Runs a clean-up statement that must not be lost to a stray `sqlite3_interrupt`: a cancel
/// decided while the operation's statement was running can be delivered an instant later, when
/// the clean-up is being parsed. SQLite forgets the interrupt when the next statement starts,
/// so trying again is enough.
pub(crate) fn execute_uninterrupted(conn: &Connection, sql: &str) -> rusqlite::Result<()> {
    const ATTEMPTS: usize = 4;
    let mut attempt = 1;
    loop {
        match conn.execute_batch(sql) {
            Err(rusqlite::Error::SqliteFailure(failure, _))
                if failure.code == rusqlite::ErrorCode::OperationInterrupted
                    && attempt < ATTEMPTS =>
            {
                attempt += 1;
            }
            result => return result,
        }
    }
}

/// Runs `body` in the savepoint `name`. On success the savepoint is released; on
/// failure it is rolled back to and released. Returns the body's result and, if
/// the rollback itself failed while a transaction is still open, `true` (the
/// connection is in an unknown state and must be treated as broken).
pub(crate) fn in_savepoint<T, E: From<rusqlite::Error>>(
    conn: &Connection,
    name: &str,
    body: impl FnOnce(&Connection) -> Result<T, E>,
) -> (Result<T, E>, bool) {
    if let Err(error) = conn.execute_batch(&format!("SAVEPOINT {name}")) {
        return (Err(error.into()), false);
    }
    let outcome = body(conn).and_then(|value| {
        conn.execute_batch(&format!("RELEASE {name}"))
            .map(|()| value)
            .map_err(E::from)
    });
    if outcome.is_ok() {
        return (outcome, false);
    }
    let rolled_back = execute_uninterrupted(conn, &format!("ROLLBACK TO {name}"))
        .and_then(|()| execute_uninterrupted(conn, &format!("RELEASE {name}")));
    // SQLite itself ends the transaction on some errors (interrupt, I/O): then
    // there is nothing left to roll back and the connection is fine.
    let broken = rolled_back.is_err() && !conn.is_autocommit();
    (outcome, broken)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE items (tenant INT NOT NULL, id INT NOT NULL, name TEXT, qty INT, PRIMARY KEY (tenant, id));
             CREATE TABLE notes (owner TEXT, body TEXT);
             CREATE TABLE dups (k INT, v INT);
             CREATE TABLE swallow (id INT);
             CREATE TRIGGER swallow_ignore BEFORE INSERT ON swallow BEGIN SELECT RAISE(IGNORE); END;
             CREATE TABLE \"we\"\"ird\" (\"a b\" INT);",
        )
        .unwrap();
        conn
    }

    fn change(value: serde_json::Value) -> SqlRowChange {
        serde_json::from_value(value).unwrap()
    }

    fn plan(table: &str, changes: Vec<serde_json::Value>) -> Plan {
        Plan {
            table: table.into(),
            changes: changes.into_iter().map(change).collect(),
        }
    }

    fn code(failure: Failure) -> &'static str {
        match failure {
            Failure::Contract { code, .. } => code,
            Failure::Sqlite(error) => panic!("sqlite error {error}"),
        }
    }

    fn count(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |row| row.get(0)).unwrap()
    }

    #[test]
    fn crud_with_composite_keys_and_zero_row_changes() {
        let conn = db();
        apply(
            &conn,
            "items",
            &change(
                json!({"op": "insert", "values": {"tenant": 1, "id": 1, "name": "a", "qty": 5}}),
            ),
        )
        .unwrap();
        apply(
            &conn,
            "items",
            &change(json!({"op": "update", "key": {"tenant": 1, "id": 1}, "values": {"qty": 6}})),
        )
        .unwrap();
        assert_eq!(
            count(&conn, "SELECT qty FROM items WHERE tenant = 1 AND id = 1"),
            6
        );
        let missing = apply(
            &conn,
            "items",
            &change(json!({"op": "update", "key": {"tenant": 1, "id": 99}, "values": {"qty": 1}})),
        )
        .unwrap_err();
        assert_eq!(code(missing), codes::MUTATION_NO_ROWS_AFFECTED);
        let missing = apply(
            &conn,
            "items",
            &change(json!({"op": "delete", "key": {"tenant": 2, "id": 1}})),
        )
        .unwrap_err();
        assert_eq!(code(missing), codes::MUTATION_NO_ROWS_AFFECTED);
        // A partial key is not the row's identity when several rows match.
        apply(
            &conn,
            "items",
            &change(json!({"op": "insert", "values": {"tenant": 1, "id": 2}})),
        )
        .unwrap();
        // The statement already ran when the count is known: the savepoint undoes it.
        let ambiguous = plan("items", vec![json!({"op": "delete", "key": {"tenant": 1}})]);
        let (failed, _) = in_savepoint(&conn, "sp", |conn| apply_all(conn, &ambiguous));
        assert_eq!(
            code(failed.unwrap_err()),
            codes::MUTATION_IDENTITY_NOT_UNIQUE
        );
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM items"), 2);
        apply(
            &conn,
            "items",
            &change(json!({"op": "delete", "key": {"tenant": 1, "id": 1}})),
        )
        .unwrap();
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM items"), 1);
    }

    #[test]
    fn a_null_key_matches_sql_null() {
        let conn = db();
        conn.execute_batch("INSERT INTO notes VALUES (NULL, 'a'), ('x', 'b')")
            .unwrap();
        apply(
            &conn,
            "notes",
            &change(json!({"op": "update", "key": {"owner": null}, "values": {"body": "z"}})),
        )
        .unwrap();
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM notes WHERE owner IS NULL AND body = 'z'"
            ),
            1
        );
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM notes WHERE owner = 'x' AND body = 'b'"
            ),
            1
        );
        apply(
            &conn,
            "notes",
            &change(json!({"op": "delete", "key": {"owner": null}})),
        )
        .unwrap();
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM notes"), 1);
    }

    #[test]
    fn keys_without_a_primary_key_must_be_unique() {
        let conn = db();
        conn.execute_batch("INSERT INTO dups VALUES (1, 1), (1, 2)")
            .unwrap();
        let update = apply(
            &conn,
            "dups",
            &change(json!({"op": "update", "key": {"k": 1}, "values": {"v": 9}})),
        )
        .unwrap_err();
        assert_eq!(code(update), codes::MUTATION_IDENTITY_NOT_UNIQUE);
    }

    #[test]
    fn an_insert_that_a_trigger_swallows_affects_no_rows() {
        let conn = db();
        let swallowed = apply(
            &conn,
            "swallow",
            &change(json!({"op": "insert", "values": {"id": 1}})),
        )
        .unwrap_err();
        assert_eq!(code(swallowed), codes::MUTATION_NO_ROWS_AFFECTED);
    }

    #[test]
    fn values_and_identifiers_are_bound_or_quoted() {
        let conn = db();
        let nasty = "x'); DROP TABLE items; --";
        apply(
            &conn,
            "items",
            &change(
                json!({"op": "insert", "values": {"tenant": 4, "id": 1, "name": nasty, "qty": 7}}),
            ),
        )
        .unwrap();
        let stored: String = conn
            .query_row("SELECT name FROM items WHERE tenant = 4", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(stored, nasty);
        apply(
            &conn,
            "items",
            &change(
                json!({"op": "insert", "values": {"tenant": 4, "id": 2, "name": [1, {"a": null}]}}),
            ),
        )
        .unwrap();
        let stored: String = conn
            .query_row("SELECT name FROM items WHERE id = 2", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&stored).unwrap(),
            json!([1, {"a": null}])
        );
        apply(
            &conn,
            "we\"ird",
            &change(json!({"op": "insert", "values": {"a b": 1}})),
        )
        .unwrap();
        apply(
            &conn,
            "items",
            &change(json!({"op": "insert", "values": {"tenant": 4, "id": 3, "qty": true}})),
        )
        .unwrap();
        assert_eq!(count(&conn, "SELECT qty FROM items WHERE id = 3"), 1);
        // A column name that is not a column is an SQL error, never an injection.
        let bad = apply(
            &conn,
            "items",
            &change(json!({"op": "insert", "values": {"nope\" = 1; --": 1}})),
        )
        .unwrap_err();
        assert!(matches!(bad, Failure::Sqlite(_)));
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM items"), 3);
    }

    #[test]
    fn empty_keys_and_values_are_bad_requests() {
        let conn = db();
        for bad in [
            json!({"op": "update", "values": {"qty": 1}}),
            json!({"op": "update", "key": {"tenant": 1}}),
            json!({"op": "delete"}),
        ] {
            assert_eq!(
                code(apply(&conn, "items", &change(bad)).unwrap_err()),
                codes::MUTATION_BAD_REQUEST
            );
        }
        apply(&conn, "dups", &change(json!({"op": "insert"}))).unwrap();
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM dups WHERE k IS NULL"), 1);
    }

    #[test]
    fn batches_roll_back_to_the_savepoint_as_a_whole() {
        let conn = db();
        let ok = plan(
            "items",
            vec![
                json!({"op": "insert", "values": {"tenant": 2, "id": 1, "qty": 1}}),
                json!({"op": "insert", "values": {"tenant": 2, "id": 2, "qty": 2}}),
                json!({"op": "update", "key": {"tenant": 2, "id": 1}, "values": {"qty": 10}}),
            ],
        );
        let (applied, broken) = in_savepoint(&conn, "sp1", |conn| apply_all(conn, &ok));
        assert_eq!(applied.unwrap(), 3);
        assert!(!broken);
        assert_eq!(count(&conn, "SELECT qty FROM items WHERE id = 1"), 10);
        let failing = plan(
            "items",
            vec![
                json!({"op": "insert", "values": {"tenant": 3, "id": 1}}),
                json!({"op": "update", "key": {"tenant": 3, "id": 99}, "values": {"qty": 2}}),
            ],
        );
        let (failed, broken) = in_savepoint(&conn, "sp2", |conn| apply_all(conn, &failing));
        let message = match failed.unwrap_err() {
            Failure::Contract { code, message } => {
                assert_eq!(code, codes::MUTATION_NO_ROWS_AFFECTED);
                message
            }
            other => panic!("{other:?}"),
        };
        assert!(message.starts_with("change 1:"), "{message}");
        assert!(!broken);
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM items WHERE tenant = 3"),
            0
        );
        assert!(
            conn.is_autocommit(),
            "the savepoint was the whole transaction"
        );
    }

    #[test]
    fn a_mutation_joins_the_users_transaction_and_never_ends_it() {
        let conn = db();
        conn.execute_batch("BEGIN; INSERT INTO items VALUES (7, 1, 'user', 1)")
            .unwrap();
        let insert = plan(
            "items",
            vec![json!({"op": "insert", "values": {"tenant": 7, "id": 2}})],
        );
        let (done, _) = in_savepoint(&conn, "sp", |conn| apply_all(conn, &insert));
        done.unwrap();
        assert!(
            !conn.is_autocommit(),
            "the user's transaction is still open"
        );
        // Failures (contract and constraint) roll back to the savepoint only.
        let missing = plan(
            "items",
            vec![json!({"op": "update", "key": {"tenant": 7, "id": 99}, "values": {"qty": 1}})],
        );
        assert_eq!(
            code(
                in_savepoint(&conn, "sp", |conn| apply_all(conn, &missing))
                    .0
                    .unwrap_err()
            ),
            codes::MUTATION_NO_ROWS_AFFECTED
        );
        let duplicate = plan(
            "items",
            vec![json!({"op": "insert", "values": {"tenant": 7, "id": 2}})],
        );
        let (failed, broken) = in_savepoint(&conn, "sp", |conn| apply_all(conn, &duplicate));
        assert!(matches!(failed.unwrap_err(), Failure::Sqlite(_)));
        assert!(!broken);
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM items WHERE tenant = 7"),
            2
        );
        conn.execute_batch("ROLLBACK").unwrap();
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM items WHERE tenant = 7"),
            0
        );
    }

    #[test]
    fn a_failed_rollback_inside_a_transaction_marks_the_connection_broken() {
        let conn = db();
        conn.execute_batch("BEGIN").unwrap();
        // The body destroys its own savepoint, so `ROLLBACK TO` cannot work while
        // the user's transaction is still open: the state is unknown.
        let (result, broken) = in_savepoint(&conn, "sp", |conn| -> Result<(), Failure> {
            conn.execute_batch("RELEASE sp")?;
            Err(contract(codes::MUTATION_FAILED, "boom"))
        });
        assert!(result.is_err());
        assert!(broken);
        // In autocommit mode a vanished savepoint means the transaction already
        // ended on its own: nothing to roll back, the connection is fine.
        conn.execute_batch("ROLLBACK").unwrap();
        let (result, broken) = in_savepoint(&conn, "sp", |conn| -> Result<(), Failure> {
            conn.execute_batch("RELEASE sp")?;
            Err(contract(codes::MUTATION_FAILED, "boom"))
        });
        assert!(result.is_err());
        assert!(!broken);
    }

    #[test]
    fn requests_are_validated() {
        let request =
            |kind: &str, object: Option<&str>, payload: serde_json::Value| MutationRequest {
                operation_id: engine_driver_kit::api::OperationId(1),
                kind: kind.into(),
                database: None,
                object: object.map(str::to_owned),
                identity: json!({}),
                payload,
            };
        let code_of = |r: MutationRequest| {
            Plan::from_request(&r)
                .err()
                .and_then(|e| e.code)
                .map(|c| c.into_owned())
        };
        assert!(Plan::from_request(&request(
            "insert",
            Some("t"),
            json!({"op": "insert", "values": {"a": 1}})
        ))
        .is_ok());
        assert!(Plan::from_request(&request(
            "batch",
            Some("t"),
            json!({"changes": [{"op": "delete", "key": {"a": 1}}]})
        ))
        .is_ok());
        assert_eq!(
            code_of(request("frobnicate", Some("t"), json!({}))).as_deref(),
            Some(codes::MUTATION_UNSUPPORTED)
        );
        assert_eq!(
            code_of(request("insert", None, json!({"op": "insert"}))).as_deref(),
            Some(codes::MUTATION_BAD_REQUEST)
        );
        assert_eq!(
            code_of(request("insert", Some("t"), json!({"op": "delete"}))).as_deref(),
            Some(codes::MUTATION_BAD_REQUEST)
        );
        assert_eq!(
            code_of(request("batch", Some("t"), json!({"changes": []}))).as_deref(),
            Some(codes::MUTATION_BAD_REQUEST)
        );
        assert_eq!(
            code_of(request("insert", Some("t"), json!("nonsense"))).as_deref(),
            Some(codes::MUTATION_BAD_REQUEST)
        );
        let mut other_db = request("insert", Some("t"), json!({"op": "insert"}));
        other_db.database = Some("other".into());
        assert_eq!(
            code_of(other_db).as_deref(),
            Some(codes::MUTATION_BAD_REQUEST)
        );
    }

    #[test]
    fn json_values_map_to_sql_parameters() {
        assert_eq!(bind_value(&json!(null)), Value::Null);
        assert_eq!(bind_value(&json!(true)), Value::Integer(1));
        assert_eq!(bind_value(&json!(-7)), Value::Integer(-7));
        assert_eq!(bind_value(&json!(1.5)), Value::Real(1.5));
        assert_eq!(bind_value(&json!(u64::MAX)), Value::Real(u64::MAX as f64));
        assert_eq!(bind_value(&json!("s")), Value::Text("s".into()));
        assert_eq!(
            bind_value(&json!({"k": [1]})),
            Value::Text("{\"k\":[1]}".into())
        );
        assert_eq!(quote_identifier("a\"b"), "\"a\"\"b\"");
    }

    // ---- storage classes --------------------------------------------------

    fn class_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE u (id INTEGER PRIMARY KEY, v);
             CREATE TABLE bl (id INTEGER PRIMARY KEY, b BLOB);
             CREATE TABLE ty (id INTEGER PRIMARY KEY, i INTEGER, r REAL, t TEXT, n NUMERIC, f BOOLEAN);
             CREATE TABLE ku (k, note);",
        )
        .unwrap();
        conn
    }

    fn update(
        conn: &Connection,
        table: &str,
        id: i64,
        column: &str,
        value: serde_json::Value,
    ) -> Result<u64, Failure> {
        apply(
            conn,
            table,
            &change(json!({"op": "update", "key": {"id": id}, "values": {column: value}})),
        )
    }

    fn insert(conn: &Connection, table: &str, values: serde_json::Value) -> Result<u64, Failure> {
        apply(
            conn,
            table,
            &change(json!({"op": "insert", "values": values})),
        )
    }

    /// `typeof(col)` and the value as text, for the row `id`.
    fn stored(conn: &Connection, table: &str, column: &str, id: i64) -> (String, String) {
        conn.query_row(
            &format!("SELECT typeof({column}), quote({column}) FROM {table} WHERE id = {id}"),
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
    }

    #[test]
    fn an_edit_keeps_the_storage_class_of_an_untyped_column() {
        let conn = class_db();
        conn.execute_batch(
            "INSERT INTO u VALUES (1, 42), (2, 1.5), (3, '7'), (4, X'AB'), (5, NULL)",
        )
        .unwrap();
        // INTEGER 42 -> 43 stays INTEGER (it used to become the text '43').
        update(&conn, "u", 1, "v", json!("43")).unwrap();
        assert_eq!(stored(&conn, "u", "v", 1), ("integer".into(), "43".into()));
        // The same when the app sent a JSON number.
        update(&conn, "u", 1, "v", json!(44)).unwrap();
        assert_eq!(stored(&conn, "u", "v", 1), ("integer".into(), "44".into()));
        // REAL stays REAL, also for a whole number.
        update(&conn, "u", 2, "v", json!("2")).unwrap();
        assert_eq!(stored(&conn, "u", "v", 2), ("real".into(), "2.0".into()));
        update(&conn, "u", 1, "v", json!("2.5")).unwrap();
        assert_eq!(stored(&conn, "u", "v", 1), ("real".into(), "2.5".into()));
        update(&conn, "u", 1, "v", json!("3")).unwrap();
        assert_eq!(stored(&conn, "u", "v", 1), ("real".into(), "3.0".into()));
        // TEXT stays TEXT, also when it looks like a number or a blob literal.
        update(&conn, "u", 3, "v", json!("8")).unwrap();
        assert_eq!(stored(&conn, "u", "v", 3), ("text".into(), "'8'".into()));
        update(&conn, "u", 3, "v", json!(9)).unwrap();
        assert_eq!(stored(&conn, "u", "v", 3), ("text".into(), "'9'".into()));
        update(&conn, "u", 3, "v", json!("0x41")).unwrap();
        assert_eq!(stored(&conn, "u", "v", 3), ("text".into(), "'0x41'".into()));
        // A NULL cell in an untyped column: the value as given.
        update(&conn, "u", 5, "v", json!("hello")).unwrap();
        assert_eq!(
            stored(&conn, "u", "v", 5),
            ("text".into(), "'hello'".into())
        );
        // An INTEGER cell given text that is no number stores that text.
        update(&conn, "u", 2, "v", json!("abc")).unwrap();
        assert_eq!(stored(&conn, "u", "v", 2), ("text".into(), "'abc'".into()));
        // A BLOB cell takes `0x` hex, nothing else.
        update(&conn, "u", 4, "v", json!("0xCAfe")).unwrap();
        assert_eq!(
            stored(&conn, "u", "v", 4),
            ("blob".into(), "X'CAFE'".into())
        );
        for bad in [json!("hello"), json!("0x1"), json!("X'AB'"), json!(7)] {
            let failure = update(&conn, "u", 4, "v", bad).unwrap_err();
            assert_eq!(code(failure), codes::MUTATION_INVALID_VALUE);
        }
        assert_eq!(
            stored(&conn, "u", "v", 4),
            ("blob".into(), "X'CAFE'".into())
        );
    }

    #[test]
    fn a_blob_declared_column_takes_hex_unless_the_cell_is_text() {
        let conn = class_db();
        conn.execute_batch("INSERT INTO bl VALUES (1, X'01'), (2, 'plain'), (3, NULL), (4, 5)")
            .unwrap();
        update(&conn, "bl", 1, "b", json!("0x")).unwrap();
        assert_eq!(stored(&conn, "bl", "b", 1), ("blob".into(), "X''".into()));
        // TEXT stays TEXT in a BLOB-declared column.
        update(&conn, "bl", 2, "b", json!("0x41")).unwrap();
        assert_eq!(
            stored(&conn, "bl", "b", 2),
            ("text".into(), "'0x41'".into())
        );
        update(&conn, "bl", 2, "b", json!("words")).unwrap();
        assert_eq!(
            stored(&conn, "bl", "b", 2),
            ("text".into(), "'words'".into())
        );
        // A NULL cell and an insert are BLOBs by the declared type.
        update(&conn, "bl", 3, "b", json!("0x0aff")).unwrap();
        assert_eq!(
            stored(&conn, "bl", "b", 3),
            ("blob".into(), "X'0AFF'".into())
        );
        assert_eq!(
            code(update(&conn, "bl", 3, "b", json!("words")).unwrap_err()),
            codes::MUTATION_INVALID_VALUE
        );
        insert(&conn, "bl", json!({"id": 10, "b": "0xDEAD"})).unwrap();
        assert_eq!(
            stored(&conn, "bl", "b", 10),
            ("blob".into(), "X'DEAD'".into())
        );
        assert_eq!(
            code(insert(&conn, "bl", json!({"id": 11, "b": "words"})).unwrap_err()),
            codes::MUTATION_INVALID_VALUE
        );
        insert(&conn, "bl", json!({"id": 12, "b": null})).unwrap();
        assert_eq!(stored(&conn, "bl", "b", 12).0, "null");
        // An INTEGER cell of the BLOB column stays INTEGER.
        update(&conn, "bl", 4, "b", json!("6")).unwrap();
        assert_eq!(stored(&conn, "bl", "b", 4), ("integer".into(), "6".into()));
    }

    #[test]
    fn real_cells_take_infinity_spellings_and_refuse_nan() {
        let conn = class_db();
        conn.execute_batch("INSERT INTO ty (id, r) VALUES (1, 1.5)")
            .unwrap();
        for (text, expected) in [
            ("inf", "9.0e+999"),
            ("+Inf", "9.0e+999"),
            ("Infinity", "9.0e+999"),
            ("+infinity", "9.0e+999"),
            ("-inf", "-9.0e+999"),
            ("-Infinity", "-9.0e+999"),
            ("2", "2.0"),
            ("1e3", "1000.0"),
        ] {
            update(&conn, "ty", 1, "r", json!(text)).unwrap();
            assert_eq!(
                stored(&conn, "ty", "r", 1),
                ("real".into(), expected.into()),
                "{text}"
            );
        }
        for nan in ["nan", "NaN"] {
            let failure = update(&conn, "ty", 1, "r", json!(nan)).unwrap_err();
            assert_eq!(code(failure), codes::MUTATION_INVALID_VALUE);
        }
        // Still the last good value: NaN was not turned into NULL.
        assert_eq!(stored(&conn, "ty", "r", 1).0, "real");
        // Not a number: the text, as for any other cell.
        update(&conn, "ty", 1, "r", json!("abc")).unwrap();
        assert_eq!(stored(&conn, "ty", "r", 1), ("text".into(), "'abc'".into()));
    }

    #[test]
    fn inserts_and_null_cells_follow_the_declared_affinity() {
        let conn = class_db();
        insert(
            &conn,
            "ty",
            json!({"id": 1, "i": "5", "r": "5", "t": "5", "n": "5", "f": "true"}),
        )
        .unwrap();
        assert_eq!(stored(&conn, "ty", "i", 1), ("integer".into(), "5".into()));
        assert_eq!(stored(&conn, "ty", "r", 1), ("real".into(), "5.0".into()));
        assert_eq!(stored(&conn, "ty", "t", 1), ("text".into(), "'5'".into()));
        assert_eq!(stored(&conn, "ty", "n", 1), ("integer".into(), "5".into()));
        assert_eq!(stored(&conn, "ty", "f", 1), ("integer".into(), "1".into()));
        insert(
            &conn,
            "ty",
            json!({"id": 2, "i": 7, "r": 1.5, "t": 8, "n": "2.5", "f": false}),
        )
        .unwrap();
        assert_eq!(stored(&conn, "ty", "i", 2), ("integer".into(), "7".into()));
        assert_eq!(stored(&conn, "ty", "r", 2), ("real".into(), "1.5".into()));
        assert_eq!(stored(&conn, "ty", "t", 2), ("text".into(), "'8'".into()));
        assert_eq!(stored(&conn, "ty", "n", 2), ("real".into(), "2.5".into()));
        assert_eq!(stored(&conn, "ty", "f", 2), ("integer".into(), "0".into()));
        // A NULL cell: the declared type again.
        conn.execute_batch("INSERT INTO ty (id) VALUES (3)")
            .unwrap();
        update(&conn, "ty", 3, "i", json!("12")).unwrap();
        update(&conn, "ty", 3, "r", json!("inf")).unwrap();
        assert_eq!(stored(&conn, "ty", "i", 3), ("integer".into(), "12".into()));
        assert_eq!(
            stored(&conn, "ty", "r", 3),
            ("real".into(), "9.0e+999".into())
        );
    }

    #[test]
    fn boolean_columns_show_and_accept_one_and_zero_and_true_and_false() {
        let conn = class_db();
        conn.execute_batch("INSERT INTO ty (id, f) VALUES (1, 1), (2, 0), (3, 'maybe')")
            .unwrap();
        for (text, expected) in [
            ("true", "1"),
            ("FALSE", "0"),
            ("1", "1"),
            ("0", "0"),
            (" True ", "1"),
        ] {
            update(&conn, "ty", 1, "f", json!(text)).unwrap();
            assert_eq!(
                stored(&conn, "ty", "f", 1),
                ("integer".into(), expected.into()),
                "{text}"
            );
        }
        update(&conn, "ty", 2, "f", json!(true)).unwrap();
        assert_eq!(stored(&conn, "ty", "f", 2), ("integer".into(), "1".into()));
        // A text cell of the column stays text.
        update(&conn, "ty", 3, "f", json!("true")).unwrap();
        assert_eq!(
            stored(&conn, "ty", "f", 3),
            ("text".into(), "'true'".into())
        );
        // `true` / `false` also find the row by key.
        apply(
            &conn,
            "ty",
            &change(json!({"op": "update", "key": {"f": "true"}, "values": {"i": 1}})),
        )
        .map(|_| ())
        .or_else(|failure| match failure {
            // Rows 1 and 2 are both 1 now.
            Failure::Contract { code, .. } if code == codes::MUTATION_IDENTITY_NOT_UNIQUE => Ok(()),
            other => Err(other),
        })
        .unwrap();
    }

    #[test]
    fn keys_find_blobs_and_the_class_the_text_stands_for() {
        let conn = class_db();
        conn.execute_batch(
            "INSERT INTO ku VALUES (X'00FF', 'blob'), (7, 'int'), ('8', 'text8'), (2.5, 'real'), ('0x41', 'texthex');",
        )
        .unwrap();
        let note_of = |key: serde_json::Value, expect: &str| {
            apply(
                &conn,
                "ku",
                &change(json!({"op": "update", "key": {"k": key}, "values": {"note": expect}})),
            )
            .unwrap();
            assert_eq!(
                count(
                    &conn,
                    &format!("SELECT COUNT(*) FROM ku WHERE note = '{expect}'")
                ),
                1,
                "{expect}"
            );
        };
        note_of(json!("0x00ff"), "b1");
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM ku WHERE typeof(k) = 'blob' AND note = 'b1'"
            ),
            1
        );
        note_of(json!("7"), "i1");
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM ku WHERE typeof(k) = 'integer' AND note = 'i1'"
            ),
            1
        );
        note_of(json!("8"), "t1");
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM ku WHERE typeof(k) = 'text' AND note = 't1'"
            ),
            1
        );
        note_of(json!("2.5"), "r1");
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM ku WHERE typeof(k) = 'real' AND note = 'r1'"
            ),
            1
        );
        // `0x41` is the text of a TEXT cell, found as text (no blob X'41' exists).
        note_of(json!("0x41"), "h1");
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM ku WHERE typeof(k) = 'text' AND note = 'h1'"
            ),
            1
        );
        // Delete by a blob key.
        apply(
            &conn,
            "ku",
            &change(json!({"op": "delete", "key": {"k": "0x00FF"}})),
        )
        .unwrap();
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM ku"), 4);
        // An INTEGER 1 and a TEXT '1' in the same column: the key text is ambiguous
        // and refused, never applied to the wrong one.
        conn.execute_batch("INSERT INTO ku VALUES (1, 'one-int'), ('1', 'one-text')")
            .unwrap();
        let ambiguous = plan("ku", vec![json!({"op": "delete", "key": {"k": "1"}})]);
        let (failed, _) = in_savepoint(&conn, "sp", |conn| apply_all(conn, &ambiguous));
        assert_eq!(
            code(failed.unwrap_err()),
            codes::MUTATION_IDENTITY_NOT_UNIQUE
        );
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM ku"), 6);
        // `01` is only ever text.
        assert_eq!(
            code(
                apply(
                    &conn,
                    "ku",
                    &change(json!({"op": "delete", "key": {"k": "01"}}))
                )
                .unwrap_err()
            ),
            codes::MUTATION_NO_ROWS_AFFECTED
        );
    }

    #[test]
    fn a_batch_edits_cells_in_their_own_classes() {
        let conn = class_db();
        conn.execute_batch("INSERT INTO u VALUES (1, 1), (2, '1')")
            .unwrap();
        let batch = plan(
            "u",
            vec![
                json!({"op": "update", "key": {"id": 1}, "values": {"v": "10"}}),
                json!({"op": "update", "key": {"id": 2}, "values": {"v": "20"}}),
            ],
        );
        let (done, _) = in_savepoint(&conn, "sp", |conn| apply_all(conn, &batch));
        done.unwrap();
        assert_eq!(stored(&conn, "u", "v", 1), ("integer".into(), "10".into()));
        assert_eq!(stored(&conn, "u", "v", 2), ("text".into(), "'20'".into()));
    }
}
