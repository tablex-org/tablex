//! `describe_object`: the Properties/DDL data of one table, view or dictionary.
//!
//! Request: a `SqlObjectRef` (`database` optional: the URI's default database,
//! else `default`; `object` required; `schema` ignored). Response: a serialized
//! `engine_driver_api::sql::TableProperties`.
//!
//! ClickHouse has none of PostgreSQL's constraints, so the contract's lists
//! carry the ClickHouse equivalents:
//!
//! * `keys`: `PRIMARY` (primary key columns), plus `ORDER BY`, `PARTITION BY`,
//!   `SAMPLE BY` and `TTL` entries of kind `other` whose `definition` is the
//!   expression and whose `columns` are its top-level terms.
//! * `indexes`: the data-skipping indices (`system.data_skipping_indices`),
//!   `method` = their type with granularity (`minmax GRANULARITY 1`).
//! * `foreign_keys`, `references`, `triggers`: empty (ClickHouse has none).
//! * `ddl`: `system.tables.create_table_query` (what `SHOW CREATE TABLE`
//!   prints: engine, partition key, sorting key, TTL, settings) under a comment
//!   line with the engine, rows, bytes and active parts, because
//!   `TableProperties` has no field for those.

use crate::http::{code, Area, HttpError};
use crate::navigator::{ColumnRow, COLUMNS_SQL};
use crate::sql::{escape_param, split_expressions};
use crate::ChConnection;
use engine_driver_kit::api::sql::{
    ConstraintKind, SqlObjectRef, TableConstraint, TableIndex, TableProperties,
};
use engine_driver_kit::BackendError;
use serde_json::Value;

pub(crate) fn describe_object(
    connection: &ChConnection,
    request: &Value,
) -> Result<Value, BackendError> {
    connection.check_open(Area::Describe)?;
    let object: SqlObjectRef = serde_json::from_value(request.clone()).map_err(|error| {
        BackendError::with_code(
            "describe.bad_request",
            format!("request is not a SqlObjectRef: {error}"),
        )
    })?;
    if object.object.is_empty() {
        return Err(BackendError::with_code(
            "describe.bad_request",
            "missing object",
        ));
    }
    let database = object
        .database
        .filter(|database| !database.is_empty())
        .or_else(|| connection.client.database.clone())
        .unwrap_or_else(|| "default".to_owned());
    let properties = load(connection, &database, &object.object)?;
    serde_json::to_value(properties).map_err(|error| BackendError::new(error.to_string()))
}

fn failed(error: HttpError) -> BackendError {
    error.into_backend(Area::Describe, false)
}

fn load(
    connection: &ChConnection,
    database: &str,
    object: &str,
) -> Result<TableProperties, BackendError> {
    let client = &connection.client;
    let params = vec![
        ("db".to_owned(), escape_param(database)),
        ("table".to_owned(), escape_param(object)),
    ];
    let tables = client
        .rows(
            "SELECT engine, partition_key, sorting_key, primary_key, sampling_key, engine_full, \
             create_table_query, total_rows, total_bytes FROM system.tables \
             WHERE database = {db:String} AND name = {table:String}",
            &params,
        )
        .or_else(|error| match error.server_code() {
            // A database that does not exist has no such object either.
            Some(code::UNKNOWN_DATABASE) => Ok(Vec::new()),
            _ => Err(error),
        })
        .map_err(failed)?;
    let Some(table) = tables.first() else {
        return Err(BackendError::with_code(
            "describe.not_found",
            format!("`{database}`.`{object}` does not exist"),
        ));
    };
    let text = |index: usize| table.get(index).and_then(Value::as_str).unwrap_or_default();
    let number = |index: usize| table.get(index).and_then(Value::as_u64);
    let engine = text(0);

    let columns = client
        .rows(COLUMNS_SQL, &params)
        .map_err(failed)?
        .iter()
        .map(|row| ColumnRow::from(row).into_column())
        .collect();

    let mut keys = Vec::new();
    let mut key = |name: &str, kind: ConstraintKind, expression: &str| {
        if !expression.trim().is_empty() {
            keys.push(TableConstraint {
                name: name.to_owned(),
                kind,
                columns: split_expressions(expression),
                definition: Some(expression.to_owned()),
            });
        }
    };
    key("PRIMARY", ConstraintKind::PrimaryKey, text(3));
    key("ORDER BY", ConstraintKind::Other, text(2));
    key("PARTITION BY", ConstraintKind::Other, text(1));
    key("SAMPLE BY", ConstraintKind::Other, text(4));
    key("TTL", ConstraintKind::Other, &ttl_of(text(5)));

    let indexes = client
        .rows(
            "SELECT name, expr, type, granularity FROM system.data_skipping_indices \
             WHERE database = {db:String} AND table = {table:String} ORDER BY name",
            &params,
        )
        .map_err(failed)?
        .iter()
        .map(|row| {
            let text = |index: usize| row.get(index).and_then(Value::as_str).unwrap_or_default();
            let granularity = row.get(3).and_then(Value::as_u64);
            TableIndex {
                name: text(0).to_owned(),
                columns: split_expressions(text(1)),
                unique: false,
                primary: false,
                method: Some(match granularity {
                    Some(granularity) => format!("{} GRANULARITY {granularity}", text(2)),
                    None => text(2).to_owned(),
                }),
                definition: Some(format!("({}) TYPE {}", text(1), text(2))),
            }
        })
        .collect();

    let parts = if engine.ends_with("MergeTree") {
        client
            .rows(
                "SELECT count(), sum(rows), sum(bytes_on_disk) FROM system.parts \
                 WHERE database = {db:String} AND table = {table:String} AND active",
                &params,
            )
            .map_err(failed)?
            .first()
            .map(|row| {
                let part = |index: usize| row.get(index).and_then(Value::as_u64).unwrap_or(0);
                (part(0), part(1), part(2))
            })
    } else {
        None
    };
    Ok(TableProperties {
        columns,
        keys: Some(keys),
        foreign_keys: Some(Vec::new()),
        indexes: Some(indexes),
        references: Some(Vec::new()),
        triggers: Some(Vec::new()),
        ddl: Some(ddl(engine, number(7), number(8), parts, text(6))),
    })
}

/// The `TTL ...` clause of an `engine_full` string (up to `SETTINGS`), if any.
fn ttl_of(engine_full: &str) -> String {
    let Some(start) = engine_full.find(" TTL ") else {
        return String::new();
    };
    let clause = &engine_full[start + " TTL ".len()..];
    clause
        .find(" SETTINGS ")
        .map_or(clause, |end| &clause[..end])
        .trim()
        .to_owned()
}

/// `create_table_query` under a comment with what `TableProperties` cannot carry.
fn ddl(
    engine: &str,
    rows: Option<u64>,
    bytes: Option<u64>,
    parts: Option<(u64, u64, u64)>,
    create: &str,
) -> String {
    let mut header = format!("-- engine: {engine}");
    if let Some((active, part_rows, part_bytes)) = parts {
        header.push_str(&format!(
            ", rows: {}, bytes: {}, active parts: {active}",
            rows.unwrap_or(part_rows),
            bytes.unwrap_or(part_bytes)
        ));
    } else if let Some(rows) = rows {
        header.push_str(&format!(", rows: {rows}"));
    }
    format!("{header}\n{create}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ttl_is_cut_out_of_engine_full() {
        assert_eq!(
            ttl_of(
                "MergeTree ORDER BY id TTL d + toIntervalDay(30) SETTINGS index_granularity = 8192"
            ),
            "d + toIntervalDay(30)"
        );
        assert_eq!(
            ttl_of("MergeTree ORDER BY id TTL d + toIntervalDay(1)"),
            "d + toIntervalDay(1)"
        );
        assert_eq!(ttl_of("MergeTree ORDER BY id SETTINGS a = 1"), "");
        assert_eq!(ttl_of(""), "");
    }

    #[test]
    fn the_ddl_header_reports_what_the_contract_has_no_field_for() {
        let merge = ddl(
            "MergeTree",
            Some(10),
            Some(2048),
            Some((3, 10, 4096)),
            "CREATE TABLE t ...",
        );
        assert_eq!(
            merge,
            "-- engine: MergeTree, rows: 10, bytes: 2048, active parts: 3\nCREATE TABLE t ..."
        );
        // Without totals from system.tables the parts decide.
        assert!(ddl("MergeTree", None, None, Some((1, 5, 7)), "x")
            .starts_with("-- engine: MergeTree, rows: 5, bytes: 7, active parts: 1"));
        assert_eq!(
            ddl("View", None, None, None, "CREATE VIEW v ..."),
            "-- engine: View\nCREATE VIEW v ..."
        );
        assert_eq!(
            ddl("Memory", Some(3), None, None, "x"),
            "-- engine: Memory, rows: 3\nx"
        );
    }
}
