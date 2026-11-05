//! `dbengine_describe_object` (capability `metadata.table_properties`).
//!
//! Request: a `SqlObjectRef` (`engine_driver_api::sql`):
//! `{"database": "app", "schema": "public", "object": "orders"}` (`database`
//! defaults to the connection's, `schema` to `public`; the navigator's
//! table/view node payloads are exactly this shape).
//!
//! A request that carries a `kind` is a `SchemaObjectRef` instead (a function,
//! procedure, sequence, trigger, type or materialized view): the response is a
//! `SchemaObjectSource` (`describe.not_found` when it is gone); see
//! [`super::schema_objects`].
//!
//! Response: a serialized `engine_driver_api::sql::TableProperties`:
//!
//! ```json
//! {"columns": [{"name", "data_type", "nullable", "primary_key",
//!               "is_generated"?, "default_expression"?, "auto_increment"?}],
//!  "keys": [{"name", "kind": "primary_key|unique|check|exclusion|other",
//!            "columns": [..], "definition"}],
//!  "foreign_keys": [{"name", "table", "columns", "referenced_table",
//!                    "referenced_columns", "on_update", "on_delete", "definition"}],
//!  "indexes": [{"name", "columns", "unique", "primary", "method", "definition"}],
//!  "references": [<foreign key shape>],   // foreign keys of OTHER tables that point here
//!  "triggers": [{"name", "timing", "events", "enabled", "definition"}],
//!  "ddl": "CREATE TABLE ..."}
//! ```
//!
//! PostgreSQL reports every list (none is `null`). Errors:
//! `describe.bad_request` (no `object`), `describe.not_found` (no such table or
//! view), `describe.failed` (server error, timeout). Runs on the metadata
//! connection, so it never waits behind a running user query.

use super::connection::PgConnection;
use crate::db::postgres::{v2_is_not_found, SqlColumn, SqlForeignKey, SqlTableProperties};
use engine_driver_kit::api::sql::{
    codes, ConstraintKind, SchemaObjectRef, SchemaObjectSource, SqlObjectRef, TableColumn,
    TableConstraint, TableForeignKey, TableIndex, TableProperties, TableTrigger,
};
use engine_driver_kit::BackendError;
use serde_json::Value;

pub(crate) fn describe_object(
    connection: &PgConnection,
    request: &Value,
) -> Result<Value, BackendError> {
    if request.get("kind").is_some() {
        return describe_schema_object(connection, request);
    }
    let object: SqlObjectRef = serde_json::from_value(request.clone()).map_err(|error| {
        BackendError::with_code(
            "describe.bad_request",
            format!("describe needs an object reference ({error})"),
        )
    })?;
    if object.object.is_empty() {
        return Err(BackendError::with_code(
            "describe.bad_request",
            "describe needs `object` (table or view)",
        ));
    }
    let schema = object
        .schema
        .as_deref()
        .filter(|schema| !schema.is_empty())
        .unwrap_or("public");
    let database = connection
        .database_or_default(object.database.as_deref())
        .to_owned();
    // On the metadata connection: never behind a running user query.
    let properties = connection
        .with_metadata("describe", "describe request", v2_is_not_found, |meta| {
            let (schema, name) = (schema.to_owned(), object.object.clone());
            async move { meta.table_properties(&database, &schema, &name).await }
        })
        .map_err(|error| {
            if error.code.is_some() {
                error
            } else if v2_is_not_found(&error.message) {
                BackendError::with_code(codes::DESCRIBE_NOT_FOUND, error.message)
            } else {
                BackendError::with_code("describe.failed", error.message)
            }
        })?;
    serde_json::to_value(to_document(properties))
        .map_err(|error| BackendError::new(error.to_string()))
}

/// The source of a function, procedure, sequence, trigger, type or materialized view.
fn describe_schema_object(connection: &PgConnection, request: &Value) -> Result<Value, BackendError> {
    let reference: SchemaObjectRef = serde_json::from_value(request.clone()).map_err(|error| {
        BackendError::with_code(
            "describe.bad_request",
            format!("describe needs a schema object reference ({error})"),
        )
    })?;
    if reference.name.is_empty() {
        return Err(BackendError::with_code(
            "describe.bad_request",
            "describe needs the object's `name`",
        ));
    }
    let schema = reference
        .schema
        .as_deref()
        .filter(|schema| !schema.is_empty())
        .unwrap_or("public");
    let database = connection
        .database_or_default(reference.database.as_deref())
        .to_owned();
    let source = connection
        .with_metadata("describe", "describe request", |_| false, |meta| {
            let (schema, reference, database) = (schema.to_owned(), reference.clone(), database.clone());
            async move { super::schema_objects::source(&meta, &database, &schema, &reference).await }
        })
        .map_err(|error| match error.code {
            Some(_) => error,
            None => BackendError::with_code("describe.failed", error.message),
        })?;
    let Some(source) = source else {
        return Err(BackendError::with_code(
            codes::DESCRIBE_NOT_FOUND,
            format!("{} `{}` does not exist in `{schema}`", reference.kind.as_str(), reference.name),
        ));
    };
    serde_json::to_value(SchemaObjectSource { source })
        .map_err(|error| BackendError::new(error.to_string()))
}

fn column(column: SqlColumn) -> TableColumn {
    TableColumn {
        name: column.name,
        data_type: column.data_type,
        nullable: column.nullable,
        primary_key: column.primary_key,
        is_generated: column.is_generated,
        default_expression: column.default_expression,
        auto_increment: column.auto_increment,
        comment: column.comment,
        attributes: None,
    }
}

pub(crate) fn column_details(source: SqlColumn) -> TableColumn {
    column(source)
}

fn to_document(properties: SqlTableProperties) -> TableProperties {
    let foreign_key = |fk: SqlForeignKey| TableForeignKey {
        name: fk.name,
        table: fk.table,
        columns: fk.columns,
        referenced_table: fk.referenced_table,
        referenced_columns: fk.referenced_columns,
        on_update: Some(fk.on_update),
        on_delete: Some(fk.on_delete),
        definition: Some(fk.definition),
    };
    TableProperties {
        columns: properties.columns.into_iter().map(column).collect(),
        keys: Some(
            properties
                .constraints
                .into_iter()
                .map(|constraint| TableConstraint {
                    name: constraint.name,
                    kind: match constraint.kind_code.as_str() {
                        "p" => ConstraintKind::PrimaryKey,
                        "u" => ConstraintKind::Unique,
                        "c" => ConstraintKind::Check,
                        "x" => ConstraintKind::Exclusion,
                        _ => ConstraintKind::Other,
                    },
                    columns: constraint.columns,
                    definition: Some(constraint.definition),
                })
                .collect(),
        ),
        foreign_keys: Some(
            properties
                .foreign_keys
                .into_iter()
                .map(foreign_key)
                .collect(),
        ),
        indexes: Some(
            properties
                .indexes
                .into_iter()
                .map(|index| TableIndex {
                    name: index.name,
                    columns: index.columns,
                    unique: index.unique,
                    primary: index.primary,
                    method: Some(index.method),
                    definition: Some(index.definition),
                })
                .collect(),
        ),
        references: Some(properties.references.into_iter().map(foreign_key).collect()),
        triggers: Some(
            properties
                .triggers
                .into_iter()
                .map(|trigger| TableTrigger {
                    name: trigger.name,
                    timing: Some(trigger.timing),
                    events: trigger.events,
                    enabled: Some(trigger.enabled),
                    definition: Some(trigger.definition),
                })
                .collect(),
        ),
        ddl: Some(properties.ddl),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::postgres::SqlConstraint;

    #[test]
    fn constraint_codes_map_to_catalog_kinds() {
        let constraint = |code: &str| SqlConstraint {
            name: "c".into(),
            kind_code: code.into(),
            columns: vec!["id".into()],
            definition: "PRIMARY KEY (id)".into(),
        };
        let document = to_document(SqlTableProperties {
            columns: Vec::new(),
            constraints: ["p", "u", "c", "x", "?"].map(constraint).into(),
            foreign_keys: Vec::new(),
            indexes: Vec::new(),
            references: Vec::new(),
            triggers: Vec::new(),
            ddl: String::new(),
        });
        let kinds: Vec<ConstraintKind> = document
            .keys
            .unwrap()
            .into_iter()
            .map(|key| key.kind)
            .collect();
        assert_eq!(
            kinds,
            [
                ConstraintKind::PrimaryKey,
                ConstraintKind::Unique,
                ConstraintKind::Check,
                ConstraintKind::Exclusion,
                ConstraintKind::Other
            ]
        );
        assert!(document.columns.is_empty());
        assert_eq!(document.references, Some(Vec::new()));
    }
}
