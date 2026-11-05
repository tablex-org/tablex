//! Schema objects of a MySQL / MariaDB database: stored functions and
//! procedures, triggers, events and (MariaDB) sequences.
//!
//! * Listing (`navigator`, `payload.kind`): `information_schema` rows of the
//!   database, with the database and the name filter as bound parameters.
//! * Source (`describe_object` with a [`SchemaObjectRef`]): the server's own
//!   `SHOW CREATE ...`. A statement of that kind cannot take parameters, so the
//!   names go in as identifiers quoted with [`push_qualified`] (backticks doubled),
//!   never as raw text.

use super::conn::describe_error;
use super::sql::push_qualified;
use super::{Dialect, MyConnection};
use engine_driver_kit::api::sql::{
    codes, SchemaObjectKind, SchemaObjectRef, SchemaObjectSource, SqlObjectRef,
};
use engine_driver_kit::api::NavigatorNode;
use engine_driver_kit::BackendError;
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Row};
use serde_json::Value;

/// `(select, name column)` of the objects of `kind` in a database: the select
/// returns `(name, table)` for the schema in `?`, `table` being `NULL` for
/// every kind but triggers. `None` for a kind the server has none of.
pub(super) fn listing(
    kind: SchemaObjectKind,
    dialect: Dialect,
) -> Option<(&'static str, &'static str)> {
    Some(match kind {
        SchemaObjectKind::Function => (
            "SELECT ROUTINE_NAME, NULL FROM information_schema.ROUTINES \
             WHERE ROUTINE_SCHEMA = ? AND ROUTINE_TYPE = 'FUNCTION'",
            "ROUTINE_NAME",
        ),
        SchemaObjectKind::Procedure => (
            "SELECT ROUTINE_NAME, NULL FROM information_schema.ROUTINES \
             WHERE ROUTINE_SCHEMA = ? AND ROUTINE_TYPE = 'PROCEDURE'",
            "ROUTINE_NAME",
        ),
        SchemaObjectKind::Trigger => (
            "SELECT TRIGGER_NAME, EVENT_OBJECT_TABLE FROM information_schema.TRIGGERS \
             WHERE TRIGGER_SCHEMA = ?",
            "TRIGGER_NAME",
        ),
        SchemaObjectKind::Event => (
            "SELECT EVENT_NAME, NULL FROM information_schema.EVENTS WHERE EVENT_SCHEMA = ?",
            "EVENT_NAME",
        ),
        SchemaObjectKind::Sequence if dialect == Dialect::MariaDb => (
            "SELECT TABLE_NAME, NULL FROM information_schema.TABLES \
             WHERE TABLE_SCHEMA = ? AND TABLE_TYPE = 'SEQUENCE'",
            "TABLE_NAME",
        ),
        SchemaObjectKind::Sequence | SchemaObjectKind::Type | SchemaObjectKind::MaterializedView => {
            return None
        }
    })
}

/// The navigator node of one object; its payload is the [`SchemaObjectRef`]
/// `describe_object` takes back. A sequence is the exception: it is a row of
/// `information_schema.TABLES` that can be selected from and described like a table,
/// so its node is table-like (a `SqlObjectRef` payload, columns below it) and the host
/// asks for its source with a [`SchemaObjectRef`] it builds from the name.
pub(super) fn node(
    database: &str,
    kind: SchemaObjectKind,
    name: String,
    table: Option<String>,
) -> NavigatorNode {
    if kind == SchemaObjectKind::Sequence {
        return sequence_node(database, name);
    }
    let label = match (&table, kind) {
        (Some(table), SchemaObjectKind::Trigger) => format!("{name} on {table}"),
        _ => name.clone(),
    };
    let reference = SchemaObjectRef {
        database: Some(database.to_owned()),
        schema: None,
        kind,
        name,
        arguments: None,
        table,
        identity: None,
    };
    NavigatorNode {
        id: format!("{}:{database}:{label}", kind.as_str()),
        kind: kind.as_str().into(),
        label,
        subtitle: None,
        icon: Some(kind.as_str().into()),
        has_children: false,
        payload: serde_json::to_value(&reference).unwrap_or(Value::Null),
        actions: vec!["view_source".into()],
    }
}

fn sequence_node(database: &str, name: String) -> NavigatorNode {
    let reference = SqlObjectRef {
        database: Some(database.to_owned()),
        schema: None,
        object: name.clone(),
    };
    NavigatorNode {
        id: format!("sequence:{database}:{name}"),
        kind: "sequence".into(),
        label: name,
        subtitle: None,
        icon: Some("sequence".into()),
        has_children: true,
        payload: serde_json::to_value(&reference).unwrap_or(Value::Null),
        actions: vec!["open_query".into(), "describe".into(), "view_source".into(), "refresh".into()],
    }
}

/// The `SHOW CREATE` statement for `reference` in `database`, `None` for a kind
/// the server has no such statement for.
fn show_create(dialect: Dialect, database: &str, reference: &SchemaObjectRef) -> Option<String> {
    let what = match reference.kind {
        SchemaObjectKind::Function => "FUNCTION",
        SchemaObjectKind::Procedure => "PROCEDURE",
        SchemaObjectKind::Trigger => "TRIGGER",
        SchemaObjectKind::Event => "EVENT",
        SchemaObjectKind::Sequence if dialect == Dialect::MariaDb => "SEQUENCE",
        _ => return None,
    };
    let mut statement = format!("SHOW CREATE {what} ");
    push_qualified(&mut statement, Some(database), &reference.name);
    Some(statement)
}

/// Server errors that mean "no such database / table / routine / trigger / event".
fn is_missing(error: &mysql_async::Error) -> bool {
    matches!(error, mysql_async::Error::Server(server) if matches!(server.code, 1049 | 1146 | 1305 | 1360 | 1539))
}

/// The `Create ...` column of a `SHOW CREATE` row: `Create Function`, `Create
/// Procedure`, `Create Event`, `Create Table` (a sequence) or, for a trigger, `SQL
/// Original Statement`.
fn source_column(row: &Row) -> Option<usize> {
    row.columns_ref().iter().position(|column| {
        let name = column.name_str();
        name.starts_with("Create ") || name == "SQL Original Statement"
    })
}

/// `describe_object` for a [`SchemaObjectRef`].
pub(super) fn describe(
    connection: &MyConnection,
    dialect: Dialect,
    request: &Value,
) -> Result<Value, BackendError> {
    let reference: SchemaObjectRef = serde_json::from_value(request.clone()).map_err(|error| {
        BackendError::with_code(
            "describe.bad_request",
            format!("request is not a SchemaObjectRef: {error}"),
        )
    })?;
    if reference.name.is_empty() {
        return Err(BackendError::with_code(
            "describe.bad_request",
            "missing object name",
        ));
    }
    let database = reference
        .database
        .clone()
        .filter(|database| !database.is_empty())
        .or_else(|| connection.current_database())
        .ok_or_else(|| BackendError::with_code("describe.bad_request", "no database selected"))?;
    let statement = show_create(dialect, &database, &reference).ok_or_else(|| {
        BackendError::with_code(
            "describe.bad_request",
            format!("this server has no {} source", reference.kind.as_str()),
        )
    })?;
    let source = connection.with_meta("describe.not_connected", async |conn| {
        source(conn, &statement, &reference, &database).await
    })?;
    serde_json::to_value(SchemaObjectSource { source })
        .map_err(|error| BackendError::new(error.to_string()))
}

async fn source(
    conn: &mut Conn,
    statement: &str,
    reference: &SchemaObjectRef,
    database: &str,
) -> Result<String, BackendError> {
    let row: Option<Row> = match conn.query_first(statement).await {
        Ok(row) => row,
        Err(error) if is_missing(&error) => None,
        Err(error) => return Err(BackendError::new(describe_error(&error))),
    };
    let Some(row) = row else {
        return Err(BackendError::with_code(
            codes::DESCRIBE_NOT_FOUND,
            format!("{} `{database}`.`{}` does not exist", reference.kind.as_str(), reference.name),
        ));
    };
    // `NULL` is a routine whose body the account may not read; text that is not UTF-8 is a
    // different failure and says so.
    let text = match source_column(&row).map(|index| row.get_opt::<Option<String>, usize>(index)) {
        Some(Some(Ok(Some(text)))) => text,
        Some(Some(Ok(None))) => {
            return Err(BackendError::with_code(
                "describe.failed",
                format!(
                    "the server did not return the source of {} `{}` (the account may lack the privilege to read it)",
                    reference.kind.as_str(),
                    reference.name
                ),
            ))
        }
        Some(Some(Err(error))) => {
            return Err(BackendError::with_code(
                "describe.failed",
                format!(
                    "the source of {} `{}` is not text the driver can read: {error}",
                    reference.kind.as_str(),
                    reference.name
                ),
            ))
        }
        Some(None) | None => {
            return Err(BackendError::with_code(
                "describe.failed",
                format!("the server's answer for {} `{}` has no source column", reference.kind.as_str(), reference.name),
            ))
        }
    };
    Ok(terminated(text))
}

/// `source` as a runnable statement: `SHOW CREATE` leaves the final `;` off.
fn terminated(mut source: String) -> String {
    let trimmed = source.trim_end().len();
    source.truncate(trimmed);
    if !source.ends_with(';') {
        source.push(';');
    }
    source
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn reference(kind: SchemaObjectKind, name: &str) -> SchemaObjectRef {
        SchemaObjectRef {
            database: Some("shop".into()),
            schema: None,
            kind,
            name: name.into(),
            arguments: None,
            table: None,
            identity: None,
        }
    }

    #[test]
    fn each_dialect_lists_only_the_kinds_its_server_has() {
        for kind in [
            SchemaObjectKind::Function,
            SchemaObjectKind::Procedure,
            SchemaObjectKind::Trigger,
            SchemaObjectKind::Event,
        ] {
            assert!(listing(kind, Dialect::MySql).is_some(), "{kind:?}");
            assert!(listing(kind, Dialect::MariaDb).is_some(), "{kind:?}");
        }
        assert!(listing(SchemaObjectKind::Sequence, Dialect::MySql).is_none());
        assert!(listing(SchemaObjectKind::Sequence, Dialect::MariaDb).is_some());
        for dialect in [Dialect::MySql, Dialect::MariaDb] {
            assert!(listing(SchemaObjectKind::Type, dialect).is_none());
            assert!(listing(SchemaObjectKind::MaterializedView, dialect).is_none());
        }
    }

    #[test]
    fn listings_bind_the_schema_and_never_carry_a_name() {
        for kind in [
            SchemaObjectKind::Function,
            SchemaObjectKind::Procedure,
            SchemaObjectKind::Trigger,
            SchemaObjectKind::Event,
            SchemaObjectKind::Sequence,
        ] {
            let (sql, name_column) = listing(kind, Dialect::MariaDb).unwrap();
            assert_eq!(sql.matches('?').count(), 1, "{sql}");
            assert!(sql.starts_with(&format!("SELECT {name_column}, ")), "{sql}");
        }
    }

    #[test]
    fn show_create_quotes_hostile_names_as_identifiers() {
        let hostile = reference(SchemaObjectKind::Function, "f`; DROP TABLE t; --");
        let sql = show_create(Dialect::MySql, "sh`op", &hostile).unwrap();
        assert_eq!(sql, "SHOW CREATE FUNCTION `sh``op`.`f``; DROP TABLE t; --`");
        assert_eq!(
            show_create(Dialect::MariaDb, "shop", &reference(SchemaObjectKind::Sequence, "s")).unwrap(),
            "SHOW CREATE SEQUENCE `shop`.`s`"
        );
        assert!(show_create(Dialect::MySql, "shop", &reference(SchemaObjectKind::Sequence, "s")).is_none());
        assert!(show_create(Dialect::MySql, "shop", &reference(SchemaObjectKind::Type, "t")).is_none());
    }

    #[test]
    fn nodes_carry_the_reference_describe_takes_back() {
        let trigger = node("shop", SchemaObjectKind::Trigger, "audit".into(), Some("orders".into()));
        assert_eq!(trigger.label, "audit on orders");
        assert_eq!(trigger.kind, "trigger");
        assert!(!trigger.has_children);
        let typed: SchemaObjectRef = serde_json::from_value(trigger.payload).unwrap();
        assert_eq!(typed.table.as_deref(), Some("orders"));
        assert_eq!(typed.database.as_deref(), Some("shop"));
        // A sequence is table-like, so it opens as data and has columns like MariaDB's tables.
        let sequence = node("shop", SchemaObjectKind::Sequence, "ids".into(), None);
        assert!(sequence.has_children && sequence.actions.contains(&"open_query".to_string()));
        assert_eq!(sequence.payload, json!({"database": "shop", "object": "ids"}));
        let event = node("shop", SchemaObjectKind::Event, "nightly".into(), None);
        assert_eq!((event.label.as_str(), event.id.as_str()), ("nightly", "event:shop:nightly"));
    }

    #[test]
    fn a_source_ends_with_one_semicolon() {
        assert_eq!(terminated("CREATE FUNCTION f() RETURNS int RETURN 1".into()), "CREATE FUNCTION f() RETURNS int RETURN 1;");
        assert_eq!(terminated("CREATE EVENT e ...;\n".into()), "CREATE EVENT e ...;");
    }
}
