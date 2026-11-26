//! `dbengine_list_nav_nodes`: databases -> schemas -> objects -> columns.
//!
//! The level is chosen from the parent node's `payload` (the host echoes it back in
//! `NavigatorRequest.payload`; `NavigatorRequest.database` mirrors `payload.database`):
//!
//! | request                              | lists                                  |
//! |--------------------------------------|----------------------------------------|
//! | no `database`                        | databases                              |
//! | `database`                           | schemas                                |
//! | `database` + `schema`                | tables and views                       |
//! | `database` + `schema` + `kind`       | the objects of one `SchemaObjectKind`  |
//! | `database` + `schema` + `object`     | columns of that object                 |
//! | `database` + `schema` + `columns: true` | the columns of every table and view of the schema |
//!
//! `columns: true` (capability `metadata.schema_columns`) is one `sys.columns` statement per page,
//! keyset-paged by `engine_driver_api::sql::schema_columns_cursor` (binary table name, then
//! `column_id`), with the details the per-table request gives.
//!
//! The databases level lists every online database; one the login may not enter carries
//! `"can_connect": false` in its payload. The schemas level leaves out `sys`,
//! `INFORMATION_SCHEMA`, `guest` and the fixed `db_*` role schemas. `payload.filter` is a
//! case-insensitive literal substring of object names (`CHARINDEX`, so `_`, `%` and `[` are
//! literal). `kind` lists functions, procedures, triggers or sequences; materialized views, types
//! and events have no counterpart and answer an empty page.
//!
//! Paging: `limit` (0 = 500, at most 5000) and an opaque `cursor` (the next offset); objects are
//! paged in SQL (`OFFSET`/`FETCH`), the other levels slice their small result.
//!
//! Node kinds (also advertised as `navigator_kinds`): `database`, `schema`, `table`, `view`,
//! `function`, `procedure`, `trigger`, `sequence`, `column`. Payloads follow the shared SQL
//! contract: `database` nodes `{"database"}`, `schema` nodes `{"database","schema"}`, table-like
//! nodes a `SqlObjectRef`, schema objects a `SchemaObjectRef`, `column` nodes a `SqlColumnNode`.

use crate::catalog::{self, flag, query, rows, sys, text};
use crate::conn::{Area, Client, MsConnection};
use engine_driver_kit::api::sql::{
    parse_schema_columns_cursor, schema_columns_cursor, SchemaObjectKind, SchemaObjectRef, SqlColumnNode, SqlObjectRef,
};
use engine_driver_kit::api::{NavigatorNode, NavigatorPage, NavigatorRequest};
use engine_driver_kit::BackendError;
use serde_json::{json, Value};

pub(crate) const NAVIGATOR_KINDS: [&str; 9] = [
    "database",
    "schema",
    "table",
    "view",
    "function",
    "procedure",
    "trigger",
    "sequence",
    "column",
];

const DEFAULT_PAGE: u64 = 500;
const MAX_PAGE: u64 = 5000;

#[derive(Debug, PartialEq, Eq)]
enum Level<'a> {
    Databases,
    Schemas {
        database: &'a str,
    },
    Objects {
        database: &'a str,
        schema: &'a str,
        filter: Option<&'a str>,
        kind: Option<SchemaObjectKind>,
    },
    Columns {
        database: &'a str,
        schema: &'a str,
        object: &'a str,
    },
    /// The columns of every table and view of the schema.
    SchemaColumns {
        database: &'a str,
        schema: &'a str,
    },
}

fn payload_str<'a>(request: &'a NavigatorRequest, key: &str) -> Option<&'a str> {
    request
        .payload
        .as_ref()?
        .get(key)?
        .as_str()
        .filter(|value| !value.is_empty())
}

fn bad_request(message: impl Into<String>) -> BackendError {
    BackendError::with_code("navigator.bad_request", message)
}

fn level(request: &NavigatorRequest) -> Result<Level<'_>, BackendError> {
    let database = request
        .database
        .as_deref()
        .filter(|database| !database.is_empty())
        .or_else(|| payload_str(request, "database"))
        .or_else(|| {
            request
                .parent
                .as_deref()
                .and_then(|parent| parent.strip_prefix("database:"))
        });
    let schema = payload_str(request, "schema");
    let object = payload_str(request, "object");
    Ok(match (database, schema, object) {
        (None, None, None) => Level::Databases,
        (Some(database), None, _) => Level::Schemas { database },
        (Some(database), Some(schema), None)
            if request.payload.as_ref().and_then(|payload| payload.get("columns")) == Some(&Value::Bool(true)) =>
        {
            Level::SchemaColumns { database, schema }
        }
        (Some(database), Some(schema), None) => Level::Objects {
            database,
            schema,
            filter: payload_str(request, "filter"),
            kind: match payload_str(request, "kind") {
                None => None,
                Some(name) => Some(
                    SchemaObjectKind::parse(name)
                        .ok_or_else(|| bad_request(format!("unknown object kind `{name}`")))?,
                ),
            },
        },
        (Some(database), Some(schema), Some(object)) => Level::Columns {
            database,
            schema,
            object,
        },
        _ => return Err(bad_request("a schema or object parent needs its database")),
    })
}

/// `(offset, limit)` of the requested page.
fn window(request: &NavigatorRequest) -> Result<(usize, usize), BackendError> {
    let offset = match request.cursor.as_deref().filter(|cursor| !cursor.is_empty()) {
        None => 0,
        Some(cursor) => cursor
            .parse::<usize>()
            .map_err(|_| bad_request("invalid navigator cursor"))?,
    };
    let limit = match request.limit {
        0 => DEFAULT_PAGE,
        limit => limit.min(MAX_PAGE),
    };
    Ok((offset, limit as usize))
}

fn page_of<T>(items: Vec<T>, offset: usize, limit: usize) -> (Vec<T>, Option<String>) {
    let total = items.len();
    let page: Vec<T> = items.into_iter().skip(offset).take(limit).collect();
    let next = (offset + page.len() < total).then(|| (offset + page.len()).to_string());
    (page, next)
}

pub(crate) fn list_nav_nodes(
    connection: &MsConnection,
    request: &NavigatorRequest,
) -> Result<NavigatorPage, BackendError> {
    let level = level(request)?;
    if let Level::SchemaColumns { database, schema } = level {
        return schema_columns_page(connection, request, database, schema);
    }
    let (offset, limit) = window(request)?;
    // On the metadata connection: never behind a running user query.
    connection.with_meta(Area::Navigator, async |client| match level {
        Level::SchemaColumns { .. } => Err(bad_request("schema columns are answered before the page window")),
        Level::Databases => {
            let found = rows(
                client,
                Area::Navigator,
                query(
                    "SELECT d.name, CAST(CASE WHEN HAS_DBACCESS(d.name) = 1 THEN 1 ELSE 0 END AS int) \
                     FROM sys.databases d WHERE d.state = 0 ORDER BY d.name",
                    &[],
                ),
            )
            .await?;
            let databases: Vec<(String, bool)> =
                found.iter().map(|row| (text(row, 0), flag(row, 1))).collect();
            let (page, next_cursor) = page_of(databases, offset, limit);
            Ok(NavigatorPage {
                nodes: page.iter().map(|(name, ok)| database_node(name, *ok)).collect(),
                next_cursor,
            })
        }
        Level::Schemas { database } => {
            let sql = format!(
                "SELECT s.name FROM {} s \
                 WHERE s.name NOT IN (N'sys', N'INFORMATION_SCHEMA', N'guest') AND s.name NOT LIKE N'db[_]%' \
                 ORDER BY s.name",
                sys(database, "schemas")
            );
            let found = rows(client, Area::Navigator, query(sql, &[])).await?;
            let names: Vec<String> = found.iter().map(|row| text(row, 0)).collect();
            let (page, next_cursor) = page_of(names, offset, limit);
            Ok(NavigatorPage {
                nodes: page.iter().map(|schema| schema_node(database, schema)).collect(),
                next_cursor,
            })
        }
        Level::Objects {
            database,
            schema,
            filter,
            kind,
        } => objects(client, database, schema, filter, kind, offset, limit).await,
        Level::Columns {
            database,
            schema,
            object,
        } => {
            let columns = catalog::columns(client, Area::Navigator, database, schema, object).await?;
            let (page, next_cursor) = page_of(columns, offset, limit);
            Ok(NavigatorPage {
                nodes: page
                    .into_iter()
                    .map(|column| column_node(database, schema, object, &column))
                    .collect(),
                next_cursor,
            })
        }
    })
}

/// One page of the schema's columns; one extra row tells whether another page exists.
fn schema_columns_page(
    connection: &MsConnection,
    request: &NavigatorRequest,
    database: &str,
    schema: &str,
) -> Result<NavigatorPage, BackendError> {
    let limit = match request.limit {
        0 => DEFAULT_PAGE,
        limit => limit.min(MAX_PAGE),
    } as usize;
    let after = match request.cursor.as_deref().filter(|cursor| !cursor.is_empty()) {
        None => None,
        Some(cursor) => Some(
            parse_schema_columns_cursor(cursor).ok_or_else(|| bad_request("invalid navigator cursor"))?,
        ),
    };
    let mut found = connection.with_meta(Area::Navigator, async |client| {
        catalog::schema_columns(client, Area::Navigator, database, schema, after, limit + 1).await
    })?;
    let next_cursor = (found.len() > limit).then(|| {
        found.truncate(limit);
        let (table, column_id, _) = &found[limit - 1];
        schema_columns_cursor(table, *column_id)
    });
    Ok(NavigatorPage {
        nodes: found
            .iter()
            .map(|(table, _, column)| column_node(database, schema, table, column))
            .collect(),
        next_cursor,
    })
}

/// How an object-level request selects: `(from/where clause with @P1 = schema, name column,
/// extra select column, node maker)`.
async fn objects(
    client: &mut Client,
    database: &str,
    schema: &str,
    filter: Option<&str>,
    kind: Option<SchemaObjectKind>,
    offset: usize,
    limit: usize,
) -> Result<NavigatorPage, BackendError> {
    // Kinds this server has no listing for answer an empty page.
    let (from, name, extra) = match kind {
        None => (
            format!(
                "{o} o JOIN {s} s ON s.schema_id = o.schema_id \
                 WHERE s.name = @P1 AND o.is_ms_shipped = 0 AND o.type IN ('U', 'V')",
                o = sys(database, "objects"),
                s = sys(database, "schemas")
            ),
            "o.name",
            "o.type",
        ),
        Some(SchemaObjectKind::Function) => (
            format!(
                "{o} o JOIN {s} s ON s.schema_id = o.schema_id \
                 WHERE s.name = @P1 AND o.is_ms_shipped = 0 AND o.type IN ('FN', 'IF', 'TF', 'FS', 'FT')",
                o = sys(database, "objects"),
                s = sys(database, "schemas")
            ),
            "o.name",
            "''",
        ),
        Some(SchemaObjectKind::Procedure) => (
            format!(
                "{o} o JOIN {s} s ON s.schema_id = o.schema_id \
                 WHERE s.name = @P1 AND o.is_ms_shipped = 0 AND o.type IN ('P', 'PC')",
                o = sys(database, "objects"),
                s = sys(database, "schemas")
            ),
            "o.name",
            "''",
        ),
        Some(SchemaObjectKind::Trigger) => (
            format!(
                "{t} tr JOIN {o} o ON o.object_id = tr.parent_id JOIN {s} s ON s.schema_id = o.schema_id \
                 WHERE s.name = @P1 AND tr.parent_class = 1 AND tr.is_ms_shipped = 0",
                t = sys(database, "triggers"),
                o = sys(database, "objects"),
                s = sys(database, "schemas")
            ),
            "tr.name",
            "o.name",
        ),
        Some(SchemaObjectKind::Sequence) => (
            format!(
                "{q} q JOIN {s} s ON s.schema_id = q.schema_id WHERE s.name = @P1",
                q = sys(database, "sequences"),
                s = sys(database, "schemas")
            ),
            "q.name",
            "''",
        ),
        Some(SchemaObjectKind::MaterializedView | SchemaObjectKind::Type | SchemaObjectKind::Event) => {
            return Ok(NavigatorPage {
                nodes: Vec::new(),
                next_cursor: None,
            })
        }
    };
    let mut sql = format!("SELECT {name}, CAST({extra} AS nvarchar(128)) FROM {from}");
    let offset_text = offset.to_string();
    // One extra row tells whether another page exists.
    let fetch_text = (limit + 1).to_string();
    let mut params: Vec<&str> = vec![schema];
    if let Some(filter) = filter {
        sql.push_str(&format!(" AND CHARINDEX(LOWER(@P{}), LOWER({name})) > 0", params.len() + 1));
        params.push(filter);
    }
    sql.push_str(&format!(" ORDER BY {name} OFFSET {offset_text} ROWS FETCH NEXT {fetch_text} ROWS ONLY"));
    let found = rows(client, Area::Navigator, query(sql, &params)).await?;
    let mut nodes: Vec<NavigatorNode> = found
        .iter()
        .map(|row| {
            let (object, extra) = (text(row, 0), text(row, 1));
            match kind {
                None => object_node(database, schema, if extra.trim() == "V" { "view" } else { "table" }, &object),
                Some(kind) => schema_object_node(
                    database,
                    schema,
                    kind,
                    &object,
                    (kind == SchemaObjectKind::Trigger).then_some(extra.as_str()),
                ),
            }
        })
        .collect();
    let next_cursor = (nodes.len() > limit).then(|| (offset + limit).to_string());
    nodes.truncate(limit);
    Ok(NavigatorPage { nodes, next_cursor })
}

/// `can_connect` is in the payload only when it is `false`: a database without the key is one the
/// user may connect to.
fn database_node(name: &str, can_connect: bool) -> NavigatorNode {
    let mut payload = json!({ "database": name });
    if !can_connect {
        payload["can_connect"] = json!(false);
    }
    NavigatorNode {
        id: format!("database:{name}"),
        kind: "database".into(),
        label: name.into(),
        subtitle: None,
        icon: Some("database".into()),
        has_children: true,
        payload,
        actions: vec!["refresh".into()],
    }
}

fn schema_node(database: &str, schema: &str) -> NavigatorNode {
    NavigatorNode {
        id: format!("schema:{database}:{schema}"),
        kind: "schema".into(),
        label: schema.into(),
        subtitle: None,
        icon: Some("schema".into()),
        has_children: true,
        payload: json!({ "database": database, "schema": schema }),
        actions: vec!["refresh".into()],
    }
}

fn object_ref(database: &str, schema: &str, object: &str) -> SqlObjectRef {
    SqlObjectRef {
        database: Some(database.to_owned()),
        schema: Some(schema.to_owned()),
        object: object.to_owned(),
    }
}

fn payload_of(value: &impl serde::Serialize) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

fn object_node(database: &str, schema: &str, kind: &'static str, name: &str) -> NavigatorNode {
    NavigatorNode {
        id: format!("{kind}:{database}:{schema}:{name}"),
        kind: kind.into(),
        label: name.into(),
        subtitle: None,
        icon: Some(kind.into()),
        has_children: true,
        payload: payload_of(&object_ref(database, schema, name)),
        actions: vec!["open_query".into(), "describe".into(), "refresh".into()],
    }
}

fn schema_object_node(
    database: &str,
    schema: &str,
    kind: SchemaObjectKind,
    name: &str,
    table: Option<&str>,
) -> NavigatorNode {
    let reference = SchemaObjectRef {
        database: Some(database.to_owned()),
        schema: Some(schema.to_owned()),
        kind,
        name: name.to_owned(),
        arguments: None,
        table: table.map(str::to_owned),
        identity: None,
    };
    NavigatorNode {
        id: format!("{}:{database}:{schema}:{name}", kind.as_str()),
        kind: kind.as_str().into(),
        label: name.into(),
        subtitle: table.map(str::to_owned),
        icon: Some(kind.as_str().into()),
        has_children: false,
        payload: payload_of(&reference),
        actions: vec!["describe".into(), "refresh".into()],
    }
}

fn column_node(database: &str, schema: &str, object: &str, column: &catalog::ColumnInfo) -> NavigatorNode {
    let details = column.detail();
    let payload = SqlColumnNode {
        object: object_ref(database, schema, object),
        column: details.name.clone(),
        details: details.clone(),
    };
    NavigatorNode {
        id: format!("column:{database}:{schema}:{object}:{}", details.name),
        kind: "column".into(),
        icon: Some(if details.primary_key { "key" } else { "column" }.into()),
        label: details.name,
        subtitle: Some(details.data_type),
        has_children: false,
        payload: payload_of(&payload),
        actions: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(parent: Option<&str>, database: Option<&str>, payload: Option<Value>) -> NavigatorRequest {
        NavigatorRequest {
            parent: parent.map(String::from),
            database: database.map(String::from),
            payload,
            cursor: None,
            limit: 0,
        }
    }

    #[test]
    fn level_follows_the_parent_payload() {
        assert_eq!(level(&request(None, None, None)).unwrap(), Level::Databases);
        assert_eq!(
            level(&request(Some("database:app"), None, None)).unwrap(),
            Level::Schemas { database: "app" }
        );
        let schema = request(
            None,
            Some("app"),
            Some(json!({"database": "app", "schema": "dbo", "filter": "ord", "kind": "procedure"})),
        );
        assert_eq!(
            level(&schema).unwrap(),
            Level::Objects {
                database: "app",
                schema: "dbo",
                filter: Some("ord"),
                kind: Some(SchemaObjectKind::Procedure)
            }
        );
        let table = request(None, None, Some(json!({"database": "app", "schema": "dbo", "object": "t"})));
        assert_eq!(
            level(&table).unwrap(),
            Level::Columns {
                database: "app",
                schema: "dbo",
                object: "t"
            }
        );
        let all = request(None, Some("app"), Some(json!({"database": "app", "schema": "dbo", "columns": true})));
        assert_eq!(level(&all).unwrap(), Level::SchemaColumns { database: "app", schema: "dbo" });
        // A table's own request wins over the flag.
        let one = request(None, Some("app"), Some(json!({"schema": "dbo", "object": "t", "columns": true})));
        assert!(matches!(level(&one).unwrap(), Level::Columns { .. }));
        let unknown = request(None, Some("app"), Some(json!({"schema": "dbo", "kind": "package"})));
        assert_eq!(level(&unknown).unwrap_err().code.as_deref(), Some("navigator.bad_request"));
        assert!(level(&request(None, None, Some(json!({"schema": "dbo"})))).is_err());
    }

    #[test]
    fn paging_slices_and_the_window_validates_its_cursor() {
        let (page, next) = page_of(vec![1, 2, 3, 4, 5], 0, 2);
        assert_eq!((page, next.as_deref()), (vec![1, 2], Some("2")));
        let (page, next) = page_of(vec![1, 2, 3, 4, 5], 4, 2);
        assert_eq!((page, next), (vec![5], None));
        let mut request = request(None, None, None);
        assert_eq!(window(&request).unwrap(), (0, 500));
        request.limit = 1_000_000;
        request.cursor = Some("40".into());
        assert_eq!(window(&request).unwrap(), (40, 5000));
        request.cursor = Some("bogus".into());
        assert!(window(&request).is_err());
    }

    #[test]
    fn nodes_carry_reusable_payloads() {
        let closed = database_node("reports", false);
        assert_eq!(closed.payload, json!({"database": "reports", "can_connect": false}));
        assert_eq!(database_node("app", true).payload, json!({"database": "app"}));
        let node = object_node("app", "dbo", "view", "v");
        assert_eq!(node.id, "view:app:dbo:v");
        let reference: SqlObjectRef = serde_json::from_value(node.payload).unwrap();
        assert_eq!(reference, object_ref("app", "dbo", "v"));
        let trigger = schema_object_node("app", "dbo", SchemaObjectKind::Trigger, "trg", Some("t"));
        let reference: SchemaObjectRef = serde_json::from_value(trigger.payload).unwrap();
        assert_eq!((reference.kind, reference.table.as_deref()), (SchemaObjectKind::Trigger, Some("t")));
        let info = catalog::ColumnInfo {
            name: "id".into(),
            data_type: "int".into(),
            base_type: "int".into(),
            nullable: false,
            primary_key: true,
            identity: true,
            computed: false,
            default: None,
            comment: None,
        };
        let column = column_node("app", "dbo", "t", &info);
        let payload: SqlColumnNode = serde_json::from_value(column.payload.clone()).unwrap();
        assert!(payload.details.primary_key && payload.details.auto_increment == Some(true));
        // Echoed back it selects the column level.
        let echoed = request(Some(&column.id), None, Some(column.payload));
        assert_eq!(
            level(&echoed).unwrap(),
            Level::Columns {
                database: "app",
                schema: "dbo",
                object: "t"
            }
        );
    }
}
