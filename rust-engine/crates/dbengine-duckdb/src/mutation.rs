//! SQL row mutations (`insert` / `update` / `delete` / `batch`), per `engine_driver_api::sql`.
//!
//! Outside a transaction of the user's, a request runs in a transaction of its own (`BEGIN` ...
//! `COMMIT`; any failure rolls it all back). Inside one it joins it, and DuckDB has no savepoint to
//! undo only this request: so an update or delete first counts the rows its key matches (none or
//! several fail before anything is written), and a batch that fails after some of its changes were
//! written reports `mutation.partially_applied` (those changes stay in the user's transaction).
//!
//! Every value is a bound parameter, sent as text, a number or a boolean: DuckDB casts it to the
//! column's type (`'2024-01-02'` to a `DATE`, `'[1,2]'` to a `LIST`), so one code path serves
//! every type. A `BLOB` column takes `0x` + hex digits, and the real bytes are bound. Keys compare
//! with `IS NOT DISTINCT FROM`, which is null-safe.

use duckdb::types::Value;
use duckdb::{params_from_iter, Connection};
use engine_driver_kit::api::sql::{codes, SqlBatch, SqlRowChange, SqlRowOp, SqlTableIdentity};
use engine_driver_kit::api::MutationRequest;
use engine_driver_kit::BackendError;
use serde_json::Map;
use std::collections::HashMap;

/// Why a request failed.
pub(crate) enum Failure {
    Contract {
        code: &'static str,
        message: String,
    },
    Duck(duckdb::Error),
    /// Inside a user transaction, change `failed` did not apply after `applied` changes had.
    Partial {
        applied: usize,
        failed: usize,
        total: usize,
        cause: Box<Failure>,
    },
}

impl From<duckdb::Error> for Failure {
    fn from(error: duckdb::Error) -> Self {
        Self::Duck(error)
    }
}

fn contract(code: &'static str, message: impl Into<String>) -> Failure {
    Failure::Contract {
        code,
        message: message.into(),
    }
}

impl Failure {
    pub(crate) fn into_error(self, cancel_requested: bool) -> BackendError {
        match self {
            Self::Contract { code, message } => BackendError::with_code(code, message),
            Self::Duck(error) => {
                let message = crate::duck_failure(&error);
                let interrupted = message.starts_with("INTERRUPT Error")
                    || (cancel_requested && message.contains("transaction is aborted"));
                if !interrupted {
                    BackendError::new(message)
                } else if cancel_requested {
                    BackendError::with_code(codes::MUTATION_CANCELLED, message)
                } else {
                    BackendError::with_code("mutation.interrupted", message).retryable()
                }
            }
            Self::Partial {
                applied,
                failed,
                total,
                cause,
            } => {
                let cause = cause.into_error(cancel_requested).message;
                let list = |range: std::ops::Range<usize>| {
                    range
                        .map(|index| index.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                BackendError::with_code(
                    codes::MUTATION_PARTIALLY_APPLIED,
                    format!(
                        "the batch stopped inside your transaction (DuckDB has no savepoints): \
                         applied: changes {}; failed: change {failed} ({cause}); not attempted: {}. \
                         Roll the transaction back to undo the applied ones.",
                        list(0..applied),
                        if failed + 1 < total { list(failed + 1..total) } else { "none".into() },
                    ),
                )
            }
        }
    }
}

/// A parsed request: the table and its changes (one for `insert`/`update`/`delete`, the whole
/// list for `batch`).
pub(crate) struct Plan {
    database: Option<String>,
    schema: Option<String>,
    table: String,
    changes: Vec<SqlRowChange>,
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
                    format!("DuckDB does not support mutation `{other}`"),
                ))
            }
        };
        let table = request
            .object
            .clone()
            .filter(|table| !table.is_empty())
            .ok_or_else(|| bad("a mutation needs the table in `object`".into()))?;
        let identity: SqlTableIdentity =
            serde_json::from_value(request.identity.clone()).unwrap_or_default();
        Ok(Self {
            database: request.database.clone().filter(|name| !name.is_empty()),
            schema: identity.schema.filter(|name| !name.is_empty()),
            table,
            changes,
        })
    }

    /// `"database"."schema"."table"`, as far as the request names them.
    fn qualified(&self) -> String {
        let mut name = String::new();
        // A database without a schema cannot qualify a table: DuckDB reads `a.b` as schema.table.
        if let (Some(database), Some(_)) = (&self.database, &self.schema) {
            name.push_str(&quote_identifier(database));
            name.push('.');
        }
        if let Some(schema) = &self.schema {
            name.push_str(&quote_identifier(schema));
            name.push('.');
        }
        name.push_str(&quote_identifier(&self.table));
        name
    }
}

/// `"name"` with embedded quotes doubled.
pub(crate) fn quote_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

pub(crate) struct Outcome {
    pub(crate) result: Result<u64, Failure>,
    /// A `ROLLBACK` failed: the transaction state is unknown.
    pub(crate) broken: bool,
}

/// Applies `plan`. `stop` tells whether the host asked to cancel (looked at between changes).
pub(crate) fn apply(
    conn: &Connection,
    plan: &Plan,
    in_user_transaction: bool,
    stop: &dyn Fn() -> bool,
) -> Outcome {
    let failed = |failure: Failure, broken: bool| Outcome {
        result: Err(failure),
        broken,
    };
    let types = match column_types(conn, plan) {
        Ok(types) => types,
        Err(failure) => return failed(failure, false),
    };
    let own = !in_user_transaction;
    if own {
        if let Err(error) = conn.execute_batch("BEGIN TRANSACTION") {
            // An interrupted `BEGIN` has begun the transaction all the same.
            let _ = conn.execute_batch("ROLLBACK");
            return failed(error.into(), false);
        }
    }
    let table = plan.qualified();
    let mut affected = 0;
    for (index, change) in plan.changes.iter().enumerate() {
        let step = if stop() {
            Err(contract(codes::MUTATION_CANCELLED, crate::CANCELLED_BEFORE_START))
        } else {
            apply_change(conn, &table, &types, change)
        };
        match step {
            Ok(rows) => affected += rows,
            Err(failure) if own => {
                return failed(failure, conn.execute_batch("ROLLBACK").is_err());
            }
            Err(failure) if index > 0 => {
                return failed(
                    Failure::Partial {
                        applied: index,
                        failed: index,
                        total: plan.changes.len(),
                        cause: Box::new(failure),
                    },
                    false,
                );
            }
            Err(failure) => return failed(failure, false),
        }
    }
    if own {
        if let Err(error) = conn.execute_batch("COMMIT") {
            // A failed commit ends the transaction: nothing was written.
            let _ = conn.execute_batch("ROLLBACK");
            return failed(error.into(), false);
        }
    }
    Outcome {
        result: Ok(affected),
        broken: false,
    }
}

/// Type name of every column of the table (`BLOB` columns take hex text).
fn column_types(conn: &Connection, plan: &Plan) -> Result<HashMap<String, String>, Failure> {
    let mut statement = conn.prepare_cached(
        "SELECT column_name, data_type FROM duckdb_columns() \
         WHERE database_name = coalesce(?, current_database()) \
           AND schema_name = coalesce(?, current_schema()) AND table_name = ?",
    )?;
    let rows = statement.query_map(
        duckdb::params![plan.database.as_deref(), plan.schema.as_deref(), plan.table.as_str()],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    )?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn apply_change(
    conn: &Connection,
    table: &str,
    types: &HashMap<String, String>,
    change: &SqlRowChange,
) -> Result<u64, Failure> {
    let bind = |map: &Map<String, serde_json::Value>| -> Result<Vec<Value>, Failure> {
        map.iter()
            .map(|(column, value)| bind_value(column, types.get(column), value))
            .collect()
    };
    match change.op {
        SqlRowOp::Insert => {
            let sql = if change.values.is_empty() {
                format!("INSERT INTO {table} DEFAULT VALUES")
            } else {
                let names = change.values.keys().map(|c| quote_identifier(c)).collect::<Vec<_>>();
                let marks = vec!["?"; names.len()].join(", ");
                format!("INSERT INTO {table} ({}) VALUES ({marks})", names.join(", "))
            };
            let rows = conn.prepare(&sql)?.execute(params_from_iter(bind(&change.values)?))?;
            exactly_one(rows)
        }
        SqlRowOp::Update | SqlRowOp::Delete => {
            if change.key.is_empty() {
                return Err(contract(
                    codes::MUTATION_BAD_REQUEST,
                    format!("a {} needs a key", change.op.kind()),
                ));
            }
            let key = bind(&change.key)?;
            let clause = change
                .key
                .keys()
                .map(|column| format!("{} IS NOT DISTINCT FROM ?", quote_identifier(column)))
                .collect::<Vec<_>>()
                .join(" AND ");
            // Count first: inside the user's transaction there is nothing to undo a write with.
            let matching: i64 = conn
                .prepare(&format!("SELECT count(*) FROM {table} WHERE {clause}"))?
                .query_row(params_from_iter(&key), |row| row.get(0))?;
            exactly_one(usize::try_from(matching).unwrap_or(usize::MAX))?;
            let (sql, mut values) = if change.op == SqlRowOp::Delete {
                (format!("DELETE FROM {table} WHERE {clause}"), Vec::new())
            } else {
                if change.values.is_empty() {
                    return Err(contract(codes::MUTATION_BAD_REQUEST, "an update needs values"));
                }
                let sets = change
                    .values
                    .keys()
                    .map(|column| format!("{} = ?", quote_identifier(column)))
                    .collect::<Vec<_>>()
                    .join(", ");
                (
                    format!("UPDATE {table} SET {sets} WHERE {clause}"),
                    bind(&change.values)?,
                )
            };
            values.extend(key);
            let rows = conn.prepare(&sql)?.execute(params_from_iter(values))?;
            exactly_one(rows)
        }
    }
}

fn exactly_one(rows: usize) -> Result<u64, Failure> {
    match rows {
        0 => Err(contract(
            codes::MUTATION_NO_ROWS_AFFECTED,
            "the change affected no row",
        )),
        1 => Ok(1),
        _ => Err(contract(
            codes::MUTATION_IDENTITY_NOT_UNIQUE,
            format!("the key matches {rows} rows"),
        )),
    }
}

/// The parameter for a JSON value: `null` is NULL, numbers keep integer-ness, text is text (a
/// `BLOB` column reads `0x` + hex), arrays and objects become JSON text.
fn bind_value(
    column: &str,
    data_type: Option<&String>,
    value: &serde_json::Value,
) -> Result<Value, Failure> {
    Ok(match value {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(flag) => Value::Boolean(*flag),
        serde_json::Value::Number(number) => {
            if let Some(integer) = number.as_i64() {
                Value::BigInt(integer)
            } else if let Some(unsigned) = number.as_u64() {
                Value::UBigInt(unsigned)
            } else {
                Value::Double(number.as_f64().unwrap_or(f64::NAN))
            }
        }
        serde_json::Value::String(text) if data_type.is_some_and(|t| t == "BLOB") => {
            Value::Blob(parse_hex_bytes(text).ok_or_else(|| {
                contract(
                    codes::MUTATION_INVALID_VALUE,
                    format!("column `{column}` is a BLOB: give `0x` followed by an even number of hex digits"),
                )
            })?)
        }
        serde_json::Value::String(text) => Value::Text(text.clone()),
        other => Value::Text(other.to_string()),
    })
}

/// `0x` + an even number of hex digits (either case); `0x` is the empty value.
fn parse_hex_bytes(text: &str) -> Option<Vec<u8>> {
    let digits = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X"))?;
    if digits.len() % 2 != 0 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    digits
        .as_bytes()
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_text_is_bytes() {
        assert_eq!(parse_hex_bytes("0x"), Some(vec![]));
        assert_eq!(parse_hex_bytes("0xAbff"), Some(vec![0xab, 0xff]));
        for bad in ["", "ab", "0x1", "0xzz", "0x+1"] {
            assert_eq!(parse_hex_bytes(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn names_are_quoted_with_doubled_quotes() {
        assert_eq!(quote_identifier("we\"ird"), "\"we\"\"ird\"");
    }

    #[test]
    fn a_table_is_qualified_by_what_the_request_names() {
        let plan = |database: Option<&str>, schema: Option<&str>| Plan {
            database: database.map(String::from),
            schema: schema.map(String::from),
            table: "t".into(),
            changes: Vec::new(),
        };
        assert_eq!(plan(None, None).qualified(), "\"t\"");
        assert_eq!(plan(None, Some("s")).qualified(), "\"s\".\"t\"");
        assert_eq!(plan(Some("d"), Some("s")).qualified(), "\"d\".\"s\".\"t\"");
        assert_eq!(plan(Some("d"), None).qualified(), "\"t\"");
    }
}
