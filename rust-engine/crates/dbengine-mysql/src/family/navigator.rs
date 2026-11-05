//! Navigator tree: databases -> tables/views -> columns, paged with an offset
//! cursor (`information_schema` rows are ordered by name / ordinal position).
//!
//! Payloads follow the shared SQL contract (`engine_driver_api::sql`):
//! `database` nodes carry `{"database"}`, `table`/`view` nodes a `SqlObjectRef`
//! (`database` + `object`; no `schema`, MySQL has none) and `column` nodes a
//! `SqlColumnNode`. A payload echoed back as `NavigatorRequest.payload` expands
//! the node.
//!
//! `payload.kind` (with a `database`) lists the objects of one
//! `SchemaObjectKind` instead (functions, procedures, triggers, events, MariaDB
//! sequences; see [`super::schema_objects`]); a kind the server has none of is an
//! empty page, an unknown one `navigator.bad_request`.
//!
//! An offset cursor, not a keyset one: `information_schema` name columns use
//! case-insensitive collations on MariaDB, where a `name > ?` keyset would skip
//! tables that differ only by case.

use super::conn::describe_error;
use super::{schema_objects, Dialect, MyConnection};
use engine_driver_kit::api::sql::{SchemaObjectKind, SqlColumnNode, SqlObjectRef, TableColumn};
use engine_driver_kit::api::{NavigatorNode, NavigatorPage, NavigatorRequest};
use engine_driver_kit::BackendError;
use mysql_async::prelude::Queryable;
use serde_json::{json, Value};
use std::fmt::Write;

/// What a navigator request asks to expand.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Target {
    Databases,
    Objects { database: String },
    /// The objects of one kind: `select` is the listing for the database in `?`,
    /// `name_column` the column that names and orders them.
    SchemaObjects {
        database: String,
        kind: SchemaObjectKind,
        select: &'static str,
        name_column: &'static str,
    },
    Columns { database: String, table: String },
}

fn text<'a>(value: Option<&'a Value>, key: &str) -> Option<&'a str> {
    value
        .and_then(|value| value.get(key))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

/// The expanded node comes from its payload (`database`, or a `SqlObjectRef`
/// with `object`); the node id (`database:<db>`, `table:<db>:<name>`,
/// `view:<db>:<name>`) is the fallback.
pub(super) fn target(request: &NavigatorRequest, dialect: Dialect) -> Target {
    let payload = request.payload.as_ref();
    let database = text(payload, "database").map(str::to_owned).or_else(|| {
        request
            .database
            .clone()
            .filter(|database| !database.is_empty())
    });
    if let (Some(database), Some(object)) = (database.clone(), text(payload, "object")) {
        return Target::Columns {
            database,
            table: object.to_owned(),
        };
    }
    let kind = text(payload, "kind").and_then(SchemaObjectKind::parse);
    // A kind this server has no listing for is answered before the target is asked for.
    if let (Some(database), Some((kind, (select, name_column)))) = (
        database.clone(),
        kind.and_then(|kind| Some((kind, schema_objects::listing(kind, dialect)?))),
    ) {
        return Target::SchemaObjects { database, kind, select, name_column };
    }
    if let Some(database) = database {
        return Target::Objects { database };
    }
    let parent = request.parent.as_deref().unwrap_or_default();
    if let Some(database) = parent.strip_prefix("database:") {
        return Target::Objects {
            database: database.to_owned(),
        };
    }
    for prefix in ["table:", "view:"] {
        if let Some(rest) = parent.strip_prefix(prefix) {
            if let Some((database, table)) = rest.split_once(':') {
                return Target::Columns {
                    database: database.to_owned(),
                    table: table.to_owned(),
                };
            }
        }
    }
    Target::Databases
}

/// Objects of a page when the request says `limit: 0`.
const DEFAULT_NAV_ROWS: u64 = 500;
/// No navigator page is larger than this.
const MAX_NAV_ROWS: u64 = 5_000;

pub(super) fn nav_rows(limit: u64) -> u64 {
    match limit {
        0 => DEFAULT_NAV_ROWS,
        limit => limit.min(MAX_NAV_ROWS),
    }
}

/// `payload.filter`: a case-insensitive literal substring of the names.
fn filter_of(request: &NavigatorRequest) -> Option<&str> {
    text(request.payload.as_ref(), "filter")
}

pub(super) fn list_nav_nodes(
    connection: &MyConnection,
    dialect: Dialect,
    request: &NavigatorRequest,
) -> Result<NavigatorPage, BackendError> {
    if let Some(name) = text(request.payload.as_ref(), "kind") {
        let Some(kind) = SchemaObjectKind::parse(name) else {
            return Err(BackendError::with_code(
                "navigator.bad_request",
                format!("unknown object kind `{name}`"),
            ));
        };
        // MySQL has no sequences, neither server has types or materialized views.
        if schema_objects::listing(kind, dialect).is_none() {
            return Ok(NavigatorPage { nodes: Vec::new(), next_cursor: None });
        }
    }
    let offset = match request.cursor.as_deref() {
        None | Some("") => 0,
        Some(cursor) => cursor.parse::<u64>().map_err(|_| {
            BackendError::with_code("navigator.bad_request", "invalid navigator cursor")
        })?,
    };
    let target = target(request, dialect);
    let limit = nav_rows(request.limit);
    // One lookahead row tells whether another page follows.
    let (sql, params) = query_for(&target, offset, limit, filter_of(request));
    let failed = |error: mysql_async::Error| BackendError::new(describe_error(&error));
    // The catalog connection: never waits behind a running user statement.
    let nodes = connection.with_meta("navigator.not_connected", async |conn| match &target {
        Target::Databases => {
            let names: Vec<String> = conn.exec(sql, params).await.map_err(failed)?;
            Ok(names.into_iter().map(database_node).collect())
        }
        Target::Objects { database } => {
            let rows: Vec<(String, String)> = conn.exec(sql, params).await.map_err(failed)?;
            Ok(rows
                .into_iter()
                .map(|(name, kind)| object_node(database, name, &kind))
                .collect())
        }
        Target::SchemaObjects { database, kind, .. } => {
            let rows: Vec<(String, Option<String>)> = conn.exec(sql, params).await.map_err(failed)?;
            Ok(rows
                .into_iter()
                .map(|(name, table)| schema_objects::node(database, *kind, name, table))
                .collect())
        }
        Target::Columns { database, table } => {
            let rows: Vec<(String, String, String, String)> =
                conn.exec(sql, params).await.map_err(failed)?;
            Ok(rows
                .into_iter()
                .map(|(name, data_type, nullable, key)| {
                    column_node(database, table, name, data_type, &nullable, &key)
                })
                .collect())
        }
    })?;
    Ok(page(nodes, offset, limit))
}

fn page(mut nodes: Vec<NavigatorNode>, offset: u64, limit: u64) -> NavigatorPage {
    let mut next_cursor = None;
    if nodes.len() as u64 > limit {
        nodes.truncate(limit as usize);
        next_cursor = Some(offset.saturating_add(limit).to_string());
    }
    NavigatorPage { nodes, next_cursor }
}

fn query_for(
    target: &Target,
    offset: u64,
    limit: u64,
    filter: Option<&str>,
) -> (String, Vec<mysql_async::Value>) {
    // `LOCATE` over `LOWER`: a literal, case-insensitive substring (`_` and `%`
    // are not wildcards, unlike `LIKE`).
    let (mut sql, mut params, name_column, filter_joiner) = match target {
        Target::Databases => (
            "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA".to_owned(),
            Vec::new(),
            "SCHEMA_NAME",
            " WHERE",
        ),
        // MariaDB sequences are rows of `TABLES` too; they have their own group.
        Target::Objects { database } => (
            "SELECT TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES \
             WHERE TABLE_SCHEMA = ? AND TABLE_TYPE <> 'SEQUENCE'"
                .to_owned(),
            vec![database.as_str().into()],
            "TABLE_NAME",
            " AND",
        ),
        Target::SchemaObjects { database, select, name_column, .. } => (
            (*select).to_owned(),
            vec![database.as_str().into()],
            *name_column,
            " AND",
        ),
        Target::Columns { database, table } => (
            "SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_KEY \
             FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?"
                .to_owned(),
            vec![database.as_str().into(), table.as_str().into()],
            "COLUMN_NAME",
            " AND",
        ),
    };
    if let Some(filter) = filter {
        let _ = write!(
            sql,
            "{filter_joiner} LOCATE(LOWER(?), LOWER({name_column})) > 0"
        );
        params.push(filter.into());
    }
    let order = match target {
        Target::Columns { .. } => "ORDINAL_POSITION",
        Target::Databases => "SCHEMA_NAME",
        Target::Objects { .. } => "TABLE_NAME",
        Target::SchemaObjects { name_column, .. } => name_column,
    };
    let _ = write!(sql, " ORDER BY {order} LIMIT {}", limit.saturating_add(1));
    if offset > 0 {
        let _ = write!(sql, " OFFSET {offset}");
    }
    (sql, params)
}

fn database_node(name: String) -> NavigatorNode {
    NavigatorNode {
        id: format!("database:{name}"),
        kind: "database".into(),
        label: name.clone(),
        subtitle: None,
        icon: Some("database".into()),
        has_children: true,
        payload: json!({ "database": name }),
        actions: vec!["refresh".into()],
    }
}

fn object_node(database: &str, name: String, table_type: &str) -> NavigatorNode {
    let kind = if table_type.contains("VIEW") {
        "view"
    } else {
        "table"
    };
    NavigatorNode {
        id: format!("{kind}:{database}:{name}"),
        kind: kind.into(),
        label: name.clone(),
        subtitle: None,
        icon: Some(kind.into()),
        has_children: true,
        payload: object_payload(database, &name),
        actions: vec!["open_query".into(), "describe".into(), "refresh".into()],
    }
}

fn object_ref(database: &str, object: &str) -> SqlObjectRef {
    SqlObjectRef {
        database: Some(database.to_owned()),
        schema: None,
        object: object.to_owned(),
    }
}

fn object_payload(database: &str, object: &str) -> Value {
    serde_json::to_value(object_ref(database, object)).unwrap_or(Value::Null)
}

fn column_node(
    database: &str,
    table: &str,
    name: String,
    data_type: String,
    nullable: &str,
    key: &str,
) -> NavigatorNode {
    NavigatorNode {
        id: format!("column:{database}:{table}:{name}"),
        kind: "column".into(),
        label: name.clone(),
        subtitle: Some(data_type.clone()),
        icon: Some("column".into()),
        has_children: false,
        payload: serde_json::to_value(SqlColumnNode {
            object: object_ref(database, table),
            column: name.clone(),
            details: TableColumn {
                name,
                data_type,
                nullable: nullable.eq_ignore_ascii_case("YES"),
                primary_key: key == "PRI",
                ..TableColumn::default()
            },
        })
        .unwrap_or(Value::Null),
        actions: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(parent: Option<&str>, payload: Option<Value>) -> NavigatorRequest {
        NavigatorRequest {
            parent: parent.map(str::to_owned),
            database: payload
                .as_ref()
                .and_then(|payload| payload.get("database"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            payload,
            cursor: None,
            limit: 0,
        }
    }

    #[test]
    fn target_prefers_the_payload_and_falls_back_to_the_node_id() {
        assert_eq!(target(&request(None, None), Dialect::MySql), Target::Databases);
        assert_eq!(
            target(&request(
                Some("database:app"),
                Some(json!({"database": "app"}))
            ), Dialect::MySql),
            Target::Objects {
                database: "app".into()
            }
        );
        assert_eq!(
            target(&request(
                Some("table:app:users"),
                Some(json!({"database": "app", "object": "users"}))
            ), Dialect::MySql),
            Target::Columns {
                database: "app".into(),
                table: "users".into()
            }
        );
        assert_eq!(
            target(&request(Some("view:app:v"), None), Dialect::MySql),
            Target::Columns {
                database: "app".into(),
                table: "v".into()
            }
        );
        assert_eq!(
            target(&request(Some("database:odd:name"), None), Dialect::MySql),
            Target::Objects {
                database: "odd:name".into()
            }
        );
    }

    #[test]
    fn a_kind_selects_its_own_listing_for_the_dialect() {
        let by_kind = |kind: &str, dialect| {
            target(
                &request(Some("database:app"), Some(json!({"database": "app", "kind": kind}))),
                dialect,
            )
        };
        let Target::SchemaObjects { kind, name_column, select, .. } = by_kind("trigger", Dialect::MySql) else {
            panic!("a trigger listing")
        };
        assert_eq!((kind, name_column), (SchemaObjectKind::Trigger, "TRIGGER_NAME"));
        assert!(select.contains("information_schema.TRIGGERS"));
        let (sql, params) = query_for(&by_kind("trigger", Dialect::MySql), 0, 10, Some("audit"));
        assert!(sql.ends_with("AND LOCATE(LOWER(?), LOWER(TRIGGER_NAME)) > 0 ORDER BY TRIGGER_NAME LIMIT 11"), "{sql}");
        assert_eq!(params.len(), 2);
        // MySQL has no sequences: that kind is not a listing there (the request is answered empty).
        assert!(matches!(by_kind("sequence", Dialect::MySql), Target::Objects { .. }));
        assert!(matches!(by_kind("sequence", Dialect::MariaDb), Target::SchemaObjects { .. }));
    }

    #[test]
    fn queries_carry_a_lookahead_row_an_offset_and_a_filter() {
        let (sql, params) = query_for(&Target::Databases, 0, 500, None);
        assert!(sql.ends_with("ORDER BY SCHEMA_NAME LIMIT 501") && params.is_empty());
        let (sql, _) = query_for(&Target::Databases, 0, 50, None);
        assert!(sql.ends_with("LIMIT 51"));
        let columns = Target::Columns {
            database: "d".into(),
            table: "t".into(),
        };
        let (sql, params) = query_for(&columns, 100, 50, None);
        assert!(sql.ends_with("ORDER BY ORDINAL_POSITION LIMIT 51 OFFSET 100"));
        assert_eq!(params.len(), 2);
        let (sql, params) = query_for(
            &Target::Objects {
                database: "d".into(),
            },
            0,
            10,
            Some("ITEM"),
        );
        assert!(
            sql.contains("AND LOCATE(LOWER(?), LOWER(TABLE_NAME)) > 0 ORDER BY"),
            "{sql}"
        );
        assert_eq!(params.len(), 2);
        let (sql, _) = query_for(&Target::Databases, 0, 10, Some("x"));
        assert!(
            sql.contains("WHERE LOCATE(LOWER(?), LOWER(SCHEMA_NAME)) > 0"),
            "{sql}"
        );
    }

    #[test]
    fn navigator_page_size_defaults_and_caps() {
        assert_eq!(nav_rows(0), 500);
        assert_eq!(nav_rows(2), 2);
        assert_eq!(nav_rows(5_000), 5_000);
        assert_eq!(nav_rows(u64::MAX), 5_000);
    }

    #[test]
    fn page_cuts_the_lookahead_row_into_a_cursor() {
        let nodes: Vec<_> = (0..4).map(|n| database_node(format!("d{n}"))).collect();
        let first = page(nodes.clone(), 0, 3);
        assert_eq!(first.nodes.len(), 3);
        assert_eq!(first.next_cursor.as_deref(), Some("3"));
        let last = page(nodes[..3].to_vec(), 3, 3);
        assert_eq!(last.nodes.len(), 3);
        assert_eq!(last.next_cursor, None);
        assert_eq!(page(nodes, 0, 10).next_cursor, None);
    }

    #[test]
    fn nodes_describe_their_kind_and_payload() {
        let view = object_node("app", "v".into(), "VIEW");
        assert_eq!(view.kind, "view");
        assert_eq!(view.id, "view:app:v");
        // Table and view nodes carry exactly a SqlObjectRef (no `table` key, no schema).
        assert_eq!(view.payload, json!({"database": "app", "object": "v"}));
        assert_eq!(view.actions, ["open_query", "describe", "refresh"]);
        assert_eq!(object_node("app", "t".into(), "BASE TABLE").kind, "table");
        assert_eq!(object_node("app", "s".into(), "SYSTEM VIEW").kind, "view");
        let column = column_node("app", "t", "id".into(), "int".into(), "NO", "PRI");
        // The payload is a SqlColumnNode and round-trips through the shared type.
        let typed: SqlColumnNode = serde_json::from_value(column.payload.clone()).unwrap();
        assert_eq!(typed.object, object_ref("app", "t"));
        assert_eq!(typed.column, "id");
        assert_eq!(typed.details.data_type, "int");
        assert!(typed.details.primary_key && !typed.details.nullable);
        assert_eq!(column.payload["primary_key"], true);
        assert_eq!(column.payload["nullable"], false);
        assert!(!column.has_children);
        assert_eq!(column.subtitle.as_deref(), Some("int"));
    }
}
