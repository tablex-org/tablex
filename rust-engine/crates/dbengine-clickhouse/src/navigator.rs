//! Navigator tree: databases -> tables / views / materialized views / dictionaries
//! -> columns, read from `system.databases`, `system.tables` and
//! `system.columns` and paged with an offset cursor.
//!
//! Payloads follow the shared SQL contract (`engine_driver_api::sql`):
//! `database` nodes carry `{"database"}`, object nodes a `SqlObjectRef`
//! (`database` + `object`; ClickHouse has no schema level) and `column` nodes a
//! `SqlColumnNode`. A payload echoed back as `NavigatorRequest.payload` expands
//! the node.
//!
//! * Object kinds: `View`, `LiveView` and `WindowView` engines are `view`,
//!   `MaterializedView` is `materialized_view`; every other engine, dictionaries
//!   included, is a `table` whose subtitle is the engine.
//! * A column's `primary_key` is `system.columns.is_in_primary_key`; `nullable`
//!   comes from the `Nullable(...)` type; `MATERIALIZED`/`ALIAS`/`EPHEMERAL`
//!   columns are `is_generated` and carry their default expression.
//! * The catalog runs on the catalog pool, never behind a running user
//!   statement. `payload.filter` is a case-insensitive literal substring
//!   (`positionCaseInsensitiveUTF8`, so `_` and `%` are not wildcards); `limit`
//!   0 is 500, capped at 5 000.

use crate::http::Area;
use crate::sql::escape_param;
use crate::types::ChType;
use crate::ChConnection;
use engine_driver_kit::api::sql::{SqlColumnNode, SqlObjectRef, TableColumn};
use engine_driver_kit::api::{NavigatorNode, NavigatorPage, NavigatorRequest};
use engine_driver_kit::BackendError;
use serde_json::{json, Value};
use std::fmt::Write;

/// What a navigator request asks to expand.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Target {
    Databases,
    Objects { database: String },
    Columns { database: String, table: String },
}

fn text<'a>(value: Option<&'a Value>, key: &str) -> Option<&'a str> {
    value
        .and_then(|value| value.get(key))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

/// The expanded node comes from its payload (`database`, or a `SqlObjectRef`
/// with `object`); the node id (`database:<db>`, `table:<db>:<name>`) is the
/// fallback.
pub(crate) fn target(request: &NavigatorRequest) -> Target {
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
    if let Some(database) = database {
        return Target::Objects { database };
    }
    let parent = request.parent.as_deref().unwrap_or_default();
    if let Some(database) = parent.strip_prefix("database:") {
        return Target::Objects {
            database: database.to_owned(),
        };
    }
    for prefix in ["table:", "view:", "materialized_view:"] {
        if let Some((database, table)) = parent
            .strip_prefix(prefix)
            .and_then(|rest| rest.split_once(':'))
        {
            return Target::Columns {
                database: database.to_owned(),
                table: table.to_owned(),
            };
        }
    }
    Target::Databases
}

/// Objects of a page when the request says `limit: 0`.
const DEFAULT_NAV_ROWS: u64 = 500;
/// No navigator page is larger than this.
const MAX_NAV_ROWS: u64 = 5_000;

pub(crate) fn nav_rows(limit: u64) -> u64 {
    match limit {
        0 => DEFAULT_NAV_ROWS,
        limit => limit.min(MAX_NAV_ROWS),
    }
}

pub(crate) fn list_nav_nodes(
    connection: &ChConnection,
    request: &NavigatorRequest,
) -> Result<NavigatorPage, BackendError> {
    connection.check_open(Area::Navigator)?;
    let offset = match request.cursor.as_deref() {
        None | Some("") => 0,
        Some(cursor) => cursor.parse::<u64>().map_err(|_| {
            BackendError::with_code("navigator.bad_request", "invalid navigator cursor")
        })?,
    };
    let target = target(request);
    let limit = nav_rows(request.limit);
    let filter = text(request.payload.as_ref(), "filter");
    // One lookahead row tells whether another page follows.
    let (sql, params) = query_for(&target, offset, limit, filter);
    let rows = connection
        .client
        .rows(&sql, &params)
        .map_err(|error| error.into_backend(Area::Navigator, false))?;
    let str_at = |row: &[Value], index: usize| {
        row.get(index)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let nodes = rows
        .iter()
        .map(|row| match &target {
            Target::Databases => database_node(str_at(row, 0)),
            Target::Objects { database } => object_node(database, str_at(row, 0), &str_at(row, 1)),
            Target::Columns { database, table } => {
                column_node(database, table, ColumnRow::from(row))
            }
        })
        .collect();
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

/// The columns query shared with `describe`.
pub(crate) const COLUMNS_SQL: &str = "SELECT name, type, is_in_primary_key, default_kind, \
     default_expression, comment FROM system.columns \
     WHERE database = {db:String} AND table = {table:String} ORDER BY position";

pub(crate) fn query_for(
    target: &Target,
    offset: u64,
    limit: u64,
    filter: Option<&str>,
) -> (String, Vec<(String, String)>) {
    let (mut sql, mut params, name_column, joiner) = match target {
        Target::Databases => (
            "SELECT name FROM system.databases".to_owned(),
            Vec::new(),
            "name",
            " WHERE",
        ),
        Target::Objects { database } => (
            "SELECT name, engine FROM system.tables WHERE database = {db:String}".to_owned(),
            vec![("db".to_owned(), escape_param(database))],
            "name",
            " AND",
        ),
        Target::Columns { database, table } => (
            COLUMNS_SQL.replace(" ORDER BY position", ""),
            vec![
                ("db".to_owned(), escape_param(database)),
                ("table".to_owned(), escape_param(table)),
            ],
            "name",
            " AND",
        ),
    };
    if let Some(filter) = filter {
        let _ = write!(
            sql,
            "{joiner} positionCaseInsensitiveUTF8({name_column}, {{filter:String}}) > 0"
        );
        params.push(("filter".to_owned(), escape_param(filter)));
    }
    let order = match target {
        Target::Columns { .. } => "position",
        _ => "name",
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

/// The navigator kind of a `system.tables` engine.
pub(crate) fn object_kind(engine: &str) -> &'static str {
    match engine {
        "View" | "LiveView" | "WindowView" => "view",
        "MaterializedView" => "materialized_view",
        _ => "table",
    }
}

fn object_node(database: &str, name: String, engine: &str) -> NavigatorNode {
    let kind = object_kind(engine);
    NavigatorNode {
        id: format!("{kind}:{database}:{name}"),
        kind: kind.into(),
        label: name.clone(),
        subtitle: (!engine.is_empty()).then(|| engine.to_owned()),
        icon: Some(kind.into()),
        has_children: true,
        payload: serde_json::to_value(object_ref(database, &name)).unwrap_or(Value::Null),
        actions: vec!["open_query".into(), "describe".into(), "refresh".into()],
    }
}

pub(crate) fn object_ref(database: &str, object: &str) -> SqlObjectRef {
    SqlObjectRef {
        database: Some(database.to_owned()),
        schema: None,
        object: object.to_owned(),
    }
}

/// One row of [`COLUMNS_SQL`].
pub(crate) struct ColumnRow {
    pub name: String,
    pub data_type: String,
    pub primary_key: bool,
    pub default_kind: String,
    pub default_expression: String,
    pub comment: String,
}

impl From<&Vec<Value>> for ColumnRow {
    fn from(row: &Vec<Value>) -> Self {
        let text = |index: usize| {
            row.get(index)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        Self {
            name: text(0),
            data_type: text(1),
            primary_key: row.get(2).and_then(Value::as_u64) == Some(1),
            default_kind: text(3),
            default_expression: text(4),
            comment: text(5),
        }
    }
}

impl ColumnRow {
    pub(crate) fn into_column(self) -> TableColumn {
        let generated = matches!(
            self.default_kind.as_str(),
            "MATERIALIZED" | "ALIAS" | "EPHEMERAL"
        );
        TableColumn {
            nullable: ChType::parse(&self.data_type).is_nullable(),
            name: self.name,
            data_type: self.data_type,
            primary_key: self.primary_key,
            is_generated: generated.then_some(true),
            default_expression: (!self.default_kind.is_empty()
                && !self.default_expression.is_empty())
            .then_some(self.default_expression),
            auto_increment: None,
            comment: Some(self.comment).filter(|comment| !comment.is_empty()),
            attributes: None,
        }
    }
}

fn column_node(database: &str, table: &str, row: ColumnRow) -> NavigatorNode {
    let details = row.into_column();
    NavigatorNode {
        id: format!("column:{database}:{table}:{}", details.name),
        kind: "column".into(),
        label: details.name.clone(),
        subtitle: Some(details.data_type.clone()),
        icon: Some("column".into()),
        has_children: false,
        payload: serde_json::to_value(SqlColumnNode {
            object: object_ref(database, table),
            column: details.name.clone(),
            details,
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
        assert_eq!(target(&request(None, None)), Target::Databases);
        assert_eq!(
            target(&request(
                Some("database:app"),
                Some(json!({"database": "app"}))
            )),
            Target::Objects {
                database: "app".into()
            }
        );
        assert_eq!(
            target(&request(
                Some("table:app:users"),
                Some(json!({"database": "app", "object": "users"}))
            )),
            Target::Columns {
                database: "app".into(),
                table: "users".into()
            }
        );
        assert_eq!(
            target(&request(Some("materialized_view:app:mv"), None)),
            Target::Columns {
                database: "app".into(),
                table: "mv".into()
            }
        );
        assert_eq!(
            target(&request(Some("database:odd:name"), None)),
            Target::Objects {
                database: "odd:name".into()
            }
        );
    }

    #[test]
    fn queries_carry_a_lookahead_row_an_offset_and_a_filter() {
        let (sql, params) = query_for(&Target::Databases, 0, 500, None);
        assert_eq!(
            sql,
            "SELECT name FROM system.databases ORDER BY name LIMIT 501"
        );
        assert!(params.is_empty());
        let columns = Target::Columns {
            database: "d".into(),
            table: "t".into(),
        };
        let (sql, params) = query_for(&columns, 100, 50, None);
        assert!(
            sql.ends_with("ORDER BY position LIMIT 51 OFFSET 100"),
            "{sql}"
        );
        assert_eq!(params.len(), 2);
        let (sql, params) = query_for(
            &Target::Objects {
                database: "d".into(),
            },
            0,
            10,
            Some("a_b\\"),
        );
        assert!(
            sql.contains(
                "AND positionCaseInsensitiveUTF8(name, {filter:String}) > 0 ORDER BY name"
            ),
            "{sql}"
        );
        // The filter is a parameter, escaped for ClickHouse's text format.
        assert_eq!(
            params.last().unwrap(),
            &("filter".to_owned(), "a_b\\\\".to_owned())
        );
        let (sql, _) = query_for(&Target::Databases, 0, 10, Some("x"));
        assert!(
            sql.contains("WHERE positionCaseInsensitiveUTF8(name,"),
            "{sql}"
        );
    }

    #[test]
    fn navigator_page_size_defaults_and_caps() {
        assert_eq!(nav_rows(0), 500);
        assert_eq!(nav_rows(2), 2);
        assert_eq!(nav_rows(u64::MAX), 5_000);
    }

    #[test]
    fn page_cuts_the_lookahead_row_into_a_cursor() {
        let nodes: Vec<_> = (0..4).map(|n| database_node(format!("d{n}"))).collect();
        let first = page(nodes.clone(), 0, 3);
        assert_eq!(
            (first.nodes.len(), first.next_cursor.as_deref()),
            (3, Some("3"))
        );
        assert_eq!(page(nodes[..3].to_vec(), 3, 3).next_cursor, None);
    }

    #[test]
    fn objects_map_engines_to_kinds() {
        for (engine, kind) in [
            ("MergeTree", "table"),
            ("Dictionary", "table"),
            ("Memory", "table"),
            ("View", "view"),
            ("LiveView", "view"),
            ("MaterializedView", "materialized_view"),
        ] {
            assert_eq!(object_kind(engine), kind, "{engine}");
        }
        let node = object_node("app", "mv".into(), "MaterializedView");
        assert_eq!(node.id, "materialized_view:app:mv");
        assert_eq!(node.payload, json!({"database": "app", "object": "mv"}));
        assert_eq!(node.subtitle.as_deref(), Some("MaterializedView"));
        assert_eq!(node.actions, ["open_query", "describe", "refresh"]);
    }

    #[test]
    fn columns_describe_keys_nullability_and_defaults() {
        let row = |values: Value| ColumnRow::from(values.as_array().unwrap());
        let key = column_node("app", "t", row(json!(["id", "UInt64", 1, "", ""])));
        let typed: SqlColumnNode = serde_json::from_value(key.payload.clone()).unwrap();
        assert_eq!(typed.object, object_ref("app", "t"));
        assert!(typed.details.primary_key && !typed.details.nullable);
        assert_eq!(typed.details.default_expression, None);
        assert_eq!(key.subtitle.as_deref(), Some("UInt64"));
        let derived = row(json!([
            "ts",
            "Nullable(DateTime)",
            0,
            "MATERIALIZED",
            "now()"
        ]))
        .into_column();
        assert!(derived.nullable && !derived.primary_key);
        assert_eq!(derived.is_generated, Some(true));
        assert_eq!(derived.default_expression.as_deref(), Some("now()"));
        let defaulted = row(json!(["n", "Int32", 0, "DEFAULT", "7"])).into_column();
        assert_eq!(defaulted.is_generated, None);
        assert_eq!(defaulted.default_expression.as_deref(), Some("7"));
    }
}
