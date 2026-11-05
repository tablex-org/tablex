//! `dbengine_list_nav_nodes`: databases -> schemas -> objects -> columns.
//!
//! The level is chosen from the parent node's `payload` (the host echoes it
//! back in `NavigatorRequest.payload`; `NavigatorRequest.database` mirrors
//! `payload.database`):
//!
//! | request                              | lists                              |
//! |--------------------------------------|------------------------------------|
//! | no `database`                        | databases                          |
//! | `database`                           | schemas (readable objects, or owned) |
//! | `database` + `schema`                | tables, views, materialized views  |
//! | `database` + `schema` + `kind`       | the objects of one `SchemaObjectKind` |
//! | `database` + `schema` + `object`     | columns of that object             |
//!
//! Extra payload key `filter`: case-insensitive literal substring on object
//! names (object level only), pushed into SQL with bound parameters.
//!
//! Extra payload key `kind` (object level only): one of
//! `engine_driver_api::sql::SchemaObjectKind`'s names. `materialized_view` lists
//! only the materialized views (table-like nodes); `function`, `procedure`,
//! `sequence`, `trigger` and `type` list those objects (see [`super::schema_objects`]),
//! and `event` (MySQL's) lists nothing. Any other value is `navigator.bad_request`.
//!
//! Catalog queries run on the connection's separate metadata connection, so a
//! navigator request never waits behind a running user query.
//!
//! Paging: `limit` (0 = 500, at most 5000) and an opaque `cursor` (the next
//! offset); `next_cursor` is `null` on the last page. Object pages are paged in
//! SQL (`LIMIT`/`OFFSET`), the other levels slice their small result.
//!
//! Node kinds (also advertised as `navigator_kinds`): `database`, `schema`,
//! `table`, `view`, `materialized_view`, `function`, `procedure`, `sequence`,
//! `trigger`, `type`, `column`. Payloads follow the shared
//! SQL contract (`engine_driver_api::sql`): `database` nodes `{"database"}`,
//! `schema` nodes `{"database","schema"}`, table-like nodes exactly a
//! `SqlObjectRef` (`database`, `schema`, `object`), `column` nodes a
//! `SqlColumnNode` (the `SqlObjectRef` keys plus `column` and the
//! `TableColumn` details). A node's payload is accepted unchanged as the next
//! request's `payload`.

use super::connection::PgConnection;
use super::schema_objects;
use crate::db::postgres::SqlObjectKind;
use engine_driver_kit::api::sql::{SchemaObjectKind, SqlColumnNode, SqlObjectRef};
use engine_driver_kit::api::{NavigatorNode, NavigatorPage, NavigatorRequest};
use engine_driver_kit::BackendError;
use serde_json::{json, Value};

pub(crate) const NAVIGATOR_KINDS: [&str; 11] = [
    "database",
    "schema",
    "table",
    "view",
    "materialized_view",
    "function",
    "procedure",
    "sequence",
    "trigger",
    "type",
    "column",
];

const DEFAULT_PAGE: u64 = 500;
const MAX_PAGE: u64 = 5000;

/// Where in the tree a request points.
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
}

fn payload_str<'a>(request: &'a NavigatorRequest, key: &str) -> Option<&'a str> {
    request
        .payload
        .as_ref()?
        .get(key)?
        .as_str()
        .filter(|value| !value.is_empty())
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
        (Some(database), Some(schema), None) => Level::Objects {
            database,
            schema,
            filter: payload_str(request, "filter"),
            kind: object_kind(request)?,
        },
        (Some(database), Some(schema), Some(object)) => Level::Columns {
            database,
            schema,
            object,
        },
        _ => {
            return Err(BackendError::with_code(
                "navigator.bad_request",
                "a schema or object parent needs its database",
            ))
        }
    })
}

/// `payload.kind`: which schema objects an object-level request lists.
fn object_kind(request: &NavigatorRequest) -> Result<Option<SchemaObjectKind>, BackendError> {
    let Some(name) = payload_str(request, "kind") else {
        return Ok(None);
    };
    SchemaObjectKind::parse(name).map(Some).ok_or_else(|| {
        BackendError::with_code(
            "navigator.bad_request",
            format!("unknown object kind `{name}`"),
        )
    })
}

/// `(offset, limit)` of the requested page.
fn window(request: &NavigatorRequest) -> Result<(usize, usize), BackendError> {
    let offset = match request
        .cursor
        .as_deref()
        .filter(|cursor| !cursor.is_empty())
    {
        None => 0,
        Some(cursor) => cursor.parse::<usize>().map_err(|_| {
            BackendError::with_code("navigator.bad_request", "invalid navigator cursor")
        })?,
    };
    let limit = match request.limit {
        0 => DEFAULT_PAGE,
        limit => limit.min(MAX_PAGE),
    };
    Ok((offset, limit as usize))
}

/// Slices a fully loaded list into one page.
fn page_of<T>(items: Vec<T>, offset: usize, limit: usize) -> (Vec<T>, Option<String>) {
    let total = items.len();
    let page: Vec<T> = items.into_iter().skip(offset).take(limit).collect();
    let next = (offset + page.len() < total).then(|| (offset + page.len()).to_string());
    (page, next)
}

pub(crate) fn list(
    connection: &PgConnection,
    request: &NavigatorRequest,
) -> Result<NavigatorPage, BackendError> {
    let level = level(request)?;
    let (offset, limit) = window(request)?;
    // On the metadata connection: never behind a running user query.
    connection.with_metadata(
        "navigator",
        "navigator request",
        |_| false,
        |inner| async move {
            match level {
                Level::Databases => {
                    let (names, next_cursor) =
                        page_of(inner.list_databases().await?, offset, limit);
                    Ok(NavigatorPage {
                        nodes: names.iter().map(|name| database_node(name)).collect(),
                        next_cursor,
                    })
                }
                Level::Schemas { database } => {
                    let (names, next_cursor) =
                        page_of(inner.list_schemas(database).await?, offset, limit);
                    Ok(NavigatorPage {
                        nodes: names
                            .iter()
                            .map(|schema| schema_node(database, schema))
                            .collect(),
                        next_cursor,
                    })
                }
                Level::Objects {
                    database,
                    schema,
                    filter,
                    kind,
                } => {
                    // One extra row tells whether another page exists.
                    let offset = i64::try_from(offset).unwrap_or(i64::MAX);
                    let fetch = i64::try_from(limit + 1).unwrap_or(i64::MAX);
                    let mut nodes = match kind {
                        None | Some(SchemaObjectKind::MaterializedView) => {
                            let relkind = kind.map(|_| "m");
                            inner
                                .v2_list_objects_page(database, schema, filter, relkind, offset, fetch)
                                .await?
                                .iter()
                                .map(|object| object_node(database, schema, object.kind, &object.name))
                                .collect()
                        }
                        Some(kind) => {
                            schema_objects::list_page(&inner, database, schema, kind, filter, offset, fetch)
                                .await?
                        }
                    };
                    let next_cursor = (nodes.len() > limit).then(|| (offset as usize + limit).to_string());
                    nodes.truncate(limit);
                    Ok(NavigatorPage { nodes, next_cursor })
                }
                Level::Columns {
                    database,
                    schema,
                    object,
                } => {
                    let (columns, next_cursor) = page_of(
                        inner.list_columns(database, schema, object).await?,
                        offset,
                        limit,
                    );
                    Ok(NavigatorPage {
                        nodes: columns
                            .into_iter()
                            .map(|column| column_node(database, schema, object, column))
                            .collect(),
                        next_cursor,
                    })
                }
            }
        },
    )
}

fn database_node(name: &str) -> NavigatorNode {
    NavigatorNode {
        id: format!("database:{name}"),
        kind: "database".into(),
        label: name.into(),
        subtitle: None,
        icon: Some("database".into()),
        has_children: true,
        payload: json!({ "database": name }),
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

fn kind_name(kind: SqlObjectKind) -> &'static str {
    match kind {
        SqlObjectKind::Table => "table",
        SqlObjectKind::View => "view",
        SqlObjectKind::MaterializedView => "materialized_view",
    }
}

/// The typed reference every table-like node carries as its payload.
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

fn object_node(database: &str, schema: &str, kind: SqlObjectKind, name: &str) -> NavigatorNode {
    let kind = kind_name(kind);
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

fn column_node(
    database: &str,
    schema: &str,
    object: &str,
    column: crate::db::postgres::SqlColumn,
) -> NavigatorNode {
    let details = super::describe::column_details(column);
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

    fn request(
        parent: Option<&str>,
        database: Option<&str>,
        payload: Option<Value>,
    ) -> NavigatorRequest {
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
            level(&request(Some("database:app"), Some("app"), None)).unwrap(),
            Level::Schemas { database: "app" }
        );
        // The parent id alone is enough for the database level.
        assert_eq!(
            level(&request(Some("database:app"), None, None)).unwrap(),
            Level::Schemas { database: "app" }
        );
        let schema = request(
            Some("schema:app:public"),
            Some("app"),
            Some(json!({"database": "app", "schema": "public", "filter": "ord"})),
        );
        assert_eq!(
            level(&schema).unwrap(),
            Level::Objects {
                database: "app",
                schema: "public",
                filter: Some("ord"),
                kind: None
            }
        );
        let functions = request(
            None,
            Some("app"),
            Some(json!({"database": "app", "schema": "public", "kind": "function"})),
        );
        assert_eq!(
            level(&functions).unwrap(),
            Level::Objects {
                database: "app",
                schema: "public",
                filter: None,
                kind: Some(SchemaObjectKind::Function)
            }
        );
        let unknown = request(
            None,
            Some("app"),
            Some(json!({"database": "app", "schema": "public", "kind": "package"})),
        );
        assert_eq!(
            level(&unknown).unwrap_err().code.as_deref(),
            Some("navigator.bad_request")
        );
        let table = request(
            None,
            None,
            Some(json!({"database": "app", "schema": "public", "object": "orders"})),
        );
        assert_eq!(
            level(&table).unwrap(),
            Level::Columns {
                database: "app",
                schema: "public",
                object: "orders"
            }
        );
        let orphan = request(None, None, Some(json!({"schema": "public"})));
        assert!(level(&orphan).is_err());
    }

    #[test]
    fn paging_slices_and_reports_the_next_cursor() {
        let (page, next) = page_of(vec![1, 2, 3, 4, 5], 0, 2);
        assert_eq!((page, next.as_deref()), (vec![1, 2], Some("2")));
        let (page, next) = page_of(vec![1, 2, 3, 4, 5], 4, 2);
        assert_eq!((page, next), (vec![5], None));
        let (page, next) = page_of(vec![1, 2], 0, 2);
        assert_eq!((page, next), (vec![1, 2], None));
        let (page, next) = page_of(Vec::<i32>::new(), 0, 5);
        assert!(page.is_empty() && next.is_none());
    }

    #[test]
    fn window_validates_the_cursor_and_clamps_the_limit() {
        let mut request = request(None, None, None);
        assert_eq!(window(&request).unwrap(), (0, 500));
        request.limit = 1_000_000;
        request.cursor = Some("40".into());
        assert_eq!(window(&request).unwrap(), (40, 5000));
        request.cursor = Some("bogus".into());
        assert!(window(&request).is_err());
    }

    #[test]
    fn object_nodes_carry_a_reusable_payload() {
        let node = object_node("app", "public", SqlObjectKind::MaterializedView, "mv");
        assert_eq!(node.kind, "materialized_view");
        assert_eq!(node.id, "materialized_view:app:public:mv");
        assert_eq!(
            node.payload,
            json!({"database": "app", "schema": "public", "object": "mv"})
        );
        let reference: SqlObjectRef = serde_json::from_value(node.payload.clone()).unwrap();
        assert_eq!(reference, object_ref("app", "public", "mv"));
        assert!(node.has_children);
    }

    #[test]
    fn column_nodes_carry_a_sql_column_node_payload() {
        let node = column_node(
            "app",
            "public",
            "orders",
            crate::db::postgres::SqlColumn {
                name: "id".into(),
                data_type: "bigint".into(),
                nullable: false,
                primary_key: true,
                is_generated: Some(false),
                default_expression: None,
                auto_increment: Some(true),
                comment: None,
            },
        );
        let payload: SqlColumnNode = serde_json::from_value(node.payload.clone()).unwrap();
        assert_eq!(payload.object, object_ref("app", "public", "orders"));
        assert_eq!(payload.column, "id");
        assert!(payload.details.primary_key);
        // Echoed back as a request payload it selects the column level.
        let echoed = request(Some(&node.id), None, Some(node.payload));
        assert_eq!(
            level(&echoed).unwrap(),
            Level::Columns {
                database: "app",
                schema: "public",
                object: "orders"
            }
        );
    }
}
