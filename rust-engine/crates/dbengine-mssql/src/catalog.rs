//! Catalog queries over the metadata connection, shared by the navigator, `describe_object` and
//! the mutation code. Every name is an identifier quoted with `sql::push_ident` or a bound
//! parameter; the `sys` views are read as `[database].sys.<view>`, so one connection serves every
//! database.

use crate::conn::{map_error, Area, Client};
use crate::sql::{push_ident, push_qualified};
use engine_driver_kit::api::sql::TableColumn;
use engine_driver_kit::BackendError;
use tiberius::{Query, Row};

/// A statement with text parameters bound as `@P1`, `@P2`, ...
pub(crate) fn query<'a>(sql: impl Into<std::borrow::Cow<'a, str>>, params: &[&'a str]) -> Query<'a> {
    let mut query = Query::new(sql);
    for param in params {
        query.bind(*param);
    }
    query
}

/// Rows of the first result set.
pub(crate) async fn rows(
    client: &mut Client,
    area: Area,
    query: Query<'_>,
) -> Result<Vec<Row>, BackendError> {
    query
        .query(client)
        .await
        .map_err(|error| map_error(area, &error))?
        .into_first_result()
        .await
        .map_err(|error| map_error(area, &error))
}

/// A cell as text (`""` for NULL or a cell that is not text).
pub(crate) fn text(row: &Row, index: usize) -> String {
    opt_text(row, index).unwrap_or_default()
}

pub(crate) fn opt_text(row: &Row, index: usize) -> Option<String> {
    row.try_get::<&str, _>(index).ok().flatten().map(str::to_owned)
}

/// A `bit` or an integer cell as a flag.
pub(crate) fn flag(row: &Row, index: usize) -> bool {
    match row.try_get::<bool, _>(index) {
        Ok(Some(flag)) => flag,
        _ => row.try_get::<i32, _>(index).ok().flatten().unwrap_or(0) != 0,
    }
}

/// An integer cell (`int`, or `bigint` read as such).
pub(crate) fn int(row: &Row, index: usize) -> i64 {
    match row.try_get::<i64, _>(index) {
        Ok(Some(value)) => value,
        _ => row.try_get::<i32, _>(index).ok().flatten().map_or(0, i64::from),
    }
}

/// The database a request means: its own, else the connection's configured one, else the
/// login's default (`DB_NAME()`).
pub(crate) async fn resolve_database(
    client: &mut Client,
    area: Area,
    requested: Option<&str>,
    configured: Option<&str>,
) -> Result<String, BackendError> {
    if let Some(database) = requested.or(configured).filter(|database| !database.is_empty()) {
        return Ok(database.to_owned());
    }
    let found = rows(client, area, Query::new("SELECT DB_NAME()")).await?;
    found
        .first()
        .and_then(|row| opt_text(row, 0))
        .ok_or_else(|| BackendError::with_code(area.failed(), "the server did not name its current database"))
}

/// `[database].sys.` prefix for the catalog view `view`.
pub(crate) fn sys(database: &str, view: &str) -> String {
    let mut out = String::new();
    push_ident(&mut out, database);
    out.push_str(".sys.");
    out.push_str(view);
    out
}

/// `[database].[schema].[object]` as `OBJECT_ID` reads it.
pub(crate) fn object_name(database: &str, schema: &str, object: &str) -> String {
    let mut out = String::new();
    push_qualified(&mut out, Some(database), Some(schema), object);
    out
}

/// What `sys.columns` says about one column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ColumnInfo {
    pub name: String,
    pub data_type: String,
    /// The base system type (`TYPE_NAME(system_type_id)`), which tells binary columns apart
    /// whatever alias type they are declared with.
    pub base_type: String,
    pub nullable: bool,
    pub primary_key: bool,
    pub identity: bool,
    pub computed: bool,
    pub default: Option<String>,
    pub comment: Option<String>,
}

impl ColumnInfo {
    pub(crate) fn detail(&self) -> TableColumn {
        TableColumn {
            name: self.name.clone(),
            data_type: self.data_type.clone(),
            nullable: self.nullable,
            primary_key: self.primary_key,
            is_generated: Some(self.computed),
            default_expression: self.default.clone(),
            auto_increment: Some(self.identity),
            comment: self.comment.clone(),
            attributes: None,
        }
    }
}

/// Which columns a [`columns_sql`] statement selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ColumnScope {
    /// One object: `@P1` is its schema, `@P2` its name.
    Object,
    /// Every table and view of the schema `@P1`, by binary table name then `column_id`, at most
    /// `limit` columns, after the column `@P2` (a table name) and `@P3` (its `column_id`, as text)
    /// when `after`. The two extra cells are the table name and the `column_id`.
    Schema { after: bool, limit: usize },
}

/// The statement behind [`columns`] and [`schema_columns`]: the same select list (cells 0 to 8) for
/// both, so a column is the same whichever way it was asked for.
pub(crate) fn columns_sql(database: &str, scope: ColumnScope) -> String {
    let (top, extra, filter, order) = match scope {
        ColumnScope::Object => (
            String::new(),
            "",
            "WHERE s.name = @P1 AND o.name = @P2".to_owned(),
            "ORDER BY c.column_id",
        ),
        ColumnScope::Schema { after, limit } => (
            format!("TOP ({limit}) "),
            ", o.name, c.column_id",
            format!(
                "WHERE s.name = @P1 AND o.is_ms_shipped = 0 AND o.type IN ('U', 'V'){}",
                if after {
                    " AND (o.name COLLATE Latin1_General_100_BIN2 > @P2 \
                      OR (o.name COLLATE Latin1_General_100_BIN2 = @P2 AND c.column_id > CAST(@P3 AS int)))"
                } else {
                    ""
                }
            ),
            "ORDER BY o.name COLLATE Latin1_General_100_BIN2, c.column_id",
        ),
    };
    format!(
        "SELECT {top}c.name, \
           CASE WHEN t.name IN (N'varchar', N'char', N'varbinary', N'binary') \
                  THEN t.name + N'(' + CASE WHEN c.max_length = -1 THEN N'max' ELSE CAST(c.max_length AS nvarchar(10)) END + N')' \
                WHEN t.name IN (N'nvarchar', N'nchar') \
                  THEN t.name + N'(' + CASE WHEN c.max_length = -1 THEN N'max' ELSE CAST(c.max_length / 2 AS nvarchar(10)) END + N')' \
                WHEN t.name IN (N'decimal', N'numeric') \
                  THEN t.name + N'(' + CAST(c.precision AS nvarchar(5)) + N',' + CAST(c.scale AS nvarchar(5)) + N')' \
                WHEN t.name IN (N'datetime2', N'datetimeoffset', N'time') \
                  THEN t.name + N'(' + CAST(c.scale AS nvarchar(5)) + N')' \
                ELSE t.name END, \
           CAST(c.is_nullable AS int), \
           CAST(CASE WHEN EXISTS (SELECT 1 FROM {ic} ic JOIN {ix} i ON i.object_id = ic.object_id AND i.index_id = ic.index_id \
                                  WHERE i.is_primary_key = 1 AND ic.object_id = c.object_id AND ic.column_id = c.column_id) \
                THEN 1 ELSE 0 END AS int), \
           CAST(c.is_identity AS int), CAST(c.is_computed AS int), \
           dc.definition, CAST(ep.value AS nvarchar(4000)), \
           ISNULL(TYPE_NAME(c.system_type_id), t.name){extra} \
         FROM {cols} c \
         JOIN {objs} o ON o.object_id = c.object_id \
         JOIN {schemas} s ON s.schema_id = o.schema_id \
         JOIN {types} t ON t.user_type_id = c.user_type_id \
         LEFT JOIN {defaults} dc ON dc.parent_object_id = c.object_id AND dc.parent_column_id = c.column_id \
         LEFT JOIN {props} ep ON ep.class = 1 AND ep.major_id = c.object_id AND ep.minor_id = c.column_id AND ep.name = N'MS_Description' \
         {filter} \
         {order}",
        ic = sys(database, "index_columns"),
        ix = sys(database, "indexes"),
        cols = sys(database, "columns"),
        objs = sys(database, "objects"),
        schemas = sys(database, "schemas"),
        types = sys(database, "types"),
        defaults = sys(database, "default_constraints"),
        props = sys(database, "extended_properties"),
    )
}

fn column_info(row: &Row) -> ColumnInfo {
    ColumnInfo {
        name: text(row, 0),
        data_type: text(row, 1),
        nullable: flag(row, 2),
        primary_key: flag(row, 3),
        identity: flag(row, 4),
        computed: flag(row, 5),
        default: opt_text(row, 6),
        comment: opt_text(row, 7).filter(|comment| !comment.is_empty()),
        base_type: text(row, 8),
    }
}

/// Columns of a table or view in ordinal order; empty when it does not exist.
pub(crate) async fn columns(
    client: &mut Client,
    area: Area,
    database: &str,
    schema: &str,
    object: &str,
) -> Result<Vec<ColumnInfo>, BackendError> {
    let sql = columns_sql(database, ColumnScope::Object);
    Ok(rows(client, area, query(sql, &[schema, object]))
        .await?
        .iter()
        .map(column_info)
        .collect())
}

/// One page of the columns of every table and view of a schema: `(table, column_id, column)`, at
/// most `limit` of them, by binary table name then `column_id`, after `after` (a table and a
/// `column_id`) when given.
pub(crate) async fn schema_columns(
    client: &mut Client,
    area: Area,
    database: &str,
    schema: &str,
    after: Option<(&str, i64)>,
    limit: usize,
) -> Result<Vec<(String, i64, ColumnInfo)>, BackendError> {
    let sql = columns_sql(database, ColumnScope::Schema { after: after.is_some(), limit });
    let column_id = after.map(|(_, column_id)| column_id.to_string());
    let mut params = vec![schema];
    if let (Some((table, _)), Some(column_id)) = (after, column_id.as_deref()) {
        params.extend([table, column_id]);
    }
    Ok(rows(client, area, query(sql, &params))
        .await?
        .iter()
        .map(|row| (text(row, 9), int(row, 10), column_info(row)))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_quoted_per_part() {
        assert_eq!(sys("my]db", "columns"), "[my]]db].sys.columns");
        assert_eq!(object_name("d", "s", "t"), "[d].[s].[t]");
    }

    #[test]
    fn both_column_statements_share_the_select_list() {
        let one = columns_sql("d", ColumnScope::Object);
        let all = columns_sql("d", ColumnScope::Schema { after: false, limit: 50 });
        let list = |sql: &str| sql[sql.find("c.name").unwrap()..sql.find(" FROM ").unwrap()].to_owned();
        assert_eq!(list(&all).trim_end_matches(", o.name, c.column_id"), list(&one));
        assert!(one.ends_with("ORDER BY c.column_id"), "{one}");
        assert!(all.starts_with("SELECT TOP (50) c.name"), "{all}");
        assert!(all.contains("WHERE s.name = @P1 AND o.is_ms_shipped = 0 AND o.type IN ('U', 'V') ORDER BY o.name COLLATE Latin1_General_100_BIN2, c.column_id"), "{all}");
        let next = columns_sql("d", ColumnScope::Schema { after: true, limit: 5 });
        assert!(next.contains("o.name COLLATE Latin1_General_100_BIN2 > @P2"), "{next}");
        assert!(next.contains("c.column_id > CAST(@P3 AS int)"), "{next}");
        assert!(next.contains("[d].sys.columns"), "{next}");
    }
}
