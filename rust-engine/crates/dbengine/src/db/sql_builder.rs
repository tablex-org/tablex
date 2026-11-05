use crate::db::CursorState;

pub(crate) fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

pub(crate) fn quote_qualified_table(schema: &str, table: &str) -> String {
    format!("{}.{}", quote_ident(schema), quote_ident(table))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditableSelectTarget {
    pub schema: Option<String>,
    pub table: String,
}

fn build_select_from_table_expr(
    table_expr: String,
    user_where: Option<&str>,
    user_order_by: Option<&str>,
    limit: i64,
) -> Result<String, String> {
    let where_clause = user_where
        .filter(|w| !w.trim().is_empty())
        .unwrap_or("TRUE");
    let order_clause = user_order_by
        .filter(|o| !o.trim().is_empty())
        .unwrap_or("1");

    let sql = format!(
        "SELECT * FROM {} WHERE {} ORDER BY {} LIMIT {}",
        table_expr, where_clause, order_clause, limit
    );

    validate_sql(&sql)?;
    Ok(sql)
}

/// Build a SELECT statement from raw user WHERE and ORDER BY clauses.
/// Validates the SQL with sqlparser to catch syntax errors early.
pub fn build_select(
    table: &str,
    user_where: Option<&str>,
    user_order_by: Option<&str>,
    limit: i64,
) -> Result<String, String> {
    build_select_from_table_expr(quote_ident(table), user_where, user_order_by, limit)
}

pub fn build_select_qualified(
    schema: &str,
    table: &str,
    user_where: Option<&str>,
    user_order_by: Option<&str>,
    limit: i64,
) -> Result<String, String> {
    build_select_from_table_expr(
        quote_qualified_table(schema, table),
        user_where,
        user_order_by,
        limit,
    )
}

/// Build a SELECT statement with keyset cursor pagination.
/// Forces ORDER BY to use the PK column and emits a typed cast for the cursor param.
/// Returns (sql, params) — params[0] is the cursor value for $1.
pub fn build_select_with_cursor(
    table: &str,
    user_where: Option<&str>,
    user_order_by: Option<&str>,
    cursor: &CursorState,
    limit: i64,
) -> Result<(String, Vec<String>), String> {
    build_select_with_cursor_from_table_expr(
        quote_ident(table),
        user_where,
        user_order_by,
        cursor,
        limit,
    )
}

pub fn build_select_with_cursor_qualified(
    schema: &str,
    table: &str,
    user_where: Option<&str>,
    user_order_by: Option<&str>,
    cursor: &CursorState,
    limit: i64,
) -> Result<(String, Vec<String>), String> {
    build_select_with_cursor_from_table_expr(
        quote_qualified_table(schema, table),
        user_where,
        user_order_by,
        cursor,
        limit,
    )
}

fn build_select_with_cursor_from_table_expr(
    table_expr: String,
    user_where: Option<&str>,
    user_order_by: Option<&str>,
    cursor: &CursorState,
    limit: i64,
) -> Result<(String, Vec<String>), String> {
    let pk = &cursor.order_column;

    // Emit typed cast to avoid "operator does not exist: bigint > text"
    let cast = if cursor.pk_type.is_empty() {
        String::new()
    } else {
        format!("::{}", cursor.pk_type)
    };
    let cursor_cond = format!("{} > $1{}", pk, cast);

    let base_where = match user_where {
        Some(w) if !w.trim().is_empty() => format!("({}) AND {}", w, cursor_cond),
        _ => cursor_cond,
    };

    // When cursor pagination is active, force ORDER BY pk to ensure stable ordering.
    // User ORDER BY is ignored to keep cursor semantics correct.
    let effective_order = if user_order_by.map(|o| !o.trim().is_empty()).unwrap_or(false) {
        user_order_by
    } else {
        None
    };
    let order_str = effective_order
        .filter(|o| !o.trim().is_empty())
        .unwrap_or(pk.as_str());

    let sql = format!(
        "SELECT * FROM {} WHERE {} ORDER BY {} LIMIT {}",
        table_expr, base_where, order_str, limit
    );
    validate_sql(&sql)?;
    Ok((sql, vec![cursor.last_value.clone()]))
}

pub fn build_count_qualified(
    schema: &str,
    table: &str,
    user_where: Option<&str>,
) -> Result<String, String> {
    let where_clause = user_where
        .filter(|w| !w.trim().is_empty())
        .unwrap_or("TRUE");
    let sql = format!(
        "SELECT COUNT(*) FROM {} WHERE {}",
        quote_qualified_table(schema, table),
        where_clause
    );
    validate_sql(&sql)?;
    Ok(sql)
}

/// Build a SELECT SQL preview string without needing a database connection.
/// Used for "Show SQL" feature — validates syntax and returns the SQL.
pub fn build_sql_preview(
    table: &str,
    user_where: Option<&str>,
    user_order_by: Option<&str>,
    limit: i64,
) -> Result<String, String> {
    let quoted_table = format!("\"{}\"", table.replace('"', "\"\""));
    let where_clause = user_where
        .filter(|w| !w.trim().is_empty())
        .unwrap_or("TRUE");
    let order_clause = user_order_by
        .filter(|o| !o.trim().is_empty())
        .unwrap_or("1");

    let sql = format!(
        "SELECT * FROM {} WHERE {} ORDER BY {} LIMIT {};",
        quoted_table, where_clause, order_clause, limit
    );

    validate_sql(&sql)?;
    Ok(sql)
}

pub fn build_sql_preview_qualified(
    schema: &str,
    table: &str,
    user_where: Option<&str>,
    user_order_by: Option<&str>,
    limit: i64,
) -> Result<String, String> {
    let quoted_table = quote_qualified_table(schema, table);
    let where_clause = user_where
        .filter(|w| !w.trim().is_empty())
        .unwrap_or("TRUE");
    let order_clause = user_order_by
        .filter(|o| !o.trim().is_empty())
        .unwrap_or("1");

    let sql = format!(
        "SELECT * FROM {} WHERE {} ORDER BY {} LIMIT {};",
        quoted_table, where_clause, order_clause, limit
    );

    validate_sql(&sql)?;
    Ok(sql)
}

pub fn build_postgres_insert_from_json_keys(
    schema: &str,
    table: &str,
    columns: &[String],
) -> Result<String, String> {
    if columns.is_empty() {
        return Err("No columns to insert".to_string());
    }

    let qualified = quote_qualified_table(schema, table);
    let insert_columns = columns
        .iter()
        .map(|column| quote_ident(column))
        .collect::<Vec<_>>()
        .join(", ");
    let select_columns = columns
        .iter()
        .map(|column| format!("tableleaf_row.{}", quote_ident(column)))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "INSERT INTO {qualified} ({insert_columns}) SELECT {select_columns} FROM json_populate_record(NULL::{qualified}, $1::json) AS tableleaf_row"
    );
    validate_sql(&sql)?;
    Ok(sql)
}

pub fn build_postgres_update_from_json(
    schema: &str,
    table: &str,
    key_column: &str,
    changed_columns: &[String],
) -> Result<String, String> {
    if changed_columns.is_empty() {
        return Err("No changed columns to update".to_string());
    }

    let qualified = quote_qualified_table(schema, table);
    let set_clause = changed_columns
        .iter()
        .map(|column| {
            let quoted = quote_ident(column);
            format!("{quoted} = tableleaf_patch.{quoted}")
        })
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "UPDATE {qualified} SET {set_clause} FROM json_populate_record(NULL::{qualified}, $1::json) AS tableleaf_patch WHERE {qualified}.{}::text = $2",
        quote_ident(key_column)
    );
    validate_sql(&sql)?;
    Ok(sql)
}

fn validate_sql(sql: &str) -> Result<(), String> {
    let dialect = sqlparser::dialect::PostgreSqlDialect {};
    sqlparser::parser::Parser::parse_sql(&dialect, sql)
        .map_err(|e| format!("SQL syntax error: {}", e))?;
    Ok(())
}

/// Return true only for raw SQL that maps unambiguously to one editable table.
///
/// This is deliberately conservative: the UI can safely enable table editing for
/// simple `SELECT * FROM table` queries, while joins, aliases, projections, CTEs,
/// subqueries, and grouped/set queries remain read-only.
pub fn is_editable_single_table_select(sql: &str) -> Result<bool, String> {
    Ok(editable_single_table_select_target(sql)?.is_some())
}

pub fn editable_single_table_select_target(
    sql: &str,
) -> Result<Option<EditableSelectTarget>, String> {
    use sqlparser::ast::{Expr, SelectItem, SetExpr, Statement, TableFactor};

    let dialect = sqlparser::dialect::PostgreSqlDialect {};
    let statements = sqlparser::parser::Parser::parse_sql(&dialect, sql)
        .map_err(|e| format!("SQL syntax error: {}", e))?;
    let [statement] = statements.as_slice() else {
        return Ok(None);
    };
    let Statement::Query(query) = statement else {
        return Ok(None);
    };
    if query.with.is_some()
        || !query.limit_by.is_empty()
        || query.offset.is_some()
        || query.fetch.is_some()
        || !query.locks.is_empty()
        || query.for_clause.is_some()
        || query.settings.is_some()
        || query.format_clause.is_some()
    {
        return Ok(None);
    }
    let SetExpr::Select(select) = query.body.as_ref() else {
        return Ok(None);
    };

    if select.distinct.is_some()
        || select.top.is_some()
        || select.into.is_some()
        || select.having.is_some()
        || select.qualify.is_some()
        || !select.lateral_views.is_empty()
        || !select.cluster_by.is_empty()
        || !select.distribute_by.is_empty()
        || !select.sort_by.is_empty()
        || !select.named_window.is_empty()
    {
        return Ok(None);
    }
    if !matches!(
        &select.group_by,
        sqlparser::ast::GroupByExpr::Expressions(exprs, modifiers)
            if exprs.is_empty() && modifiers.is_empty()
    ) {
        return Ok(None);
    }
    if !matches!(select.projection.as_slice(), [SelectItem::Wildcard(_)]) {
        return Ok(None);
    }
    let [from] = select.from.as_slice() else {
        return Ok(None);
    };
    if !from.joins.is_empty() {
        return Ok(None);
    }
    let TableFactor::Table {
        name, alias, args, ..
    } = &from.relation
    else {
        return Ok(None);
    };
    if alias.is_some() || args.is_some() {
        return Ok(None);
    }

    if let Some(limit) = &query.limit {
        if !matches!(
            limit,
            Expr::Value(_) | Expr::Identifier(_) | Expr::CompoundIdentifier(_)
        ) {
            return Ok(None);
        }
    }

    Ok(parse_editable_object_name(&name.to_string()))
}

fn parse_editable_object_name(display: &str) -> Option<EditableSelectTarget> {
    let parts = split_sql_object_name(display);
    match parts.as_slice() {
        [table] => Some(EditableSelectTarget {
            schema: None,
            table: table.clone(),
        }),
        [schema, table] => Some(EditableSelectTarget {
            schema: Some(schema.clone()),
            table: table.clone(),
        }),
        _ => None,
    }
}

fn split_sql_object_name(display: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut chars = display.chars().peekable();
    let mut in_quotes = false;

    while let Some(ch) = chars.next() {
        match ch {
            '"' => {
                if in_quotes && chars.peek() == Some(&'"') {
                    current.push('"');
                    chars.next();
                } else {
                    in_quotes = !in_quotes;
                }
            }
            '.' if !in_quotes => {
                parts.push(current.trim().to_string());
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() || display.ends_with('.') {
        parts.push(current.trim().to_string());
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_select_basic() {
        let sql = build_select("users", None, None, 100).unwrap();
        assert_eq!(
            sql,
            "SELECT * FROM \"users\" WHERE TRUE ORDER BY 1 LIMIT 100"
        );
    }

    #[test]
    fn build_select_with_where() {
        let sql = build_select("users", Some("state = 1"), None, 100).unwrap();
        assert_eq!(
            sql,
            "SELECT * FROM \"users\" WHERE state = 1 ORDER BY 1 LIMIT 100"
        );
    }

    #[test]
    fn build_select_with_order_by() {
        let sql = build_select("users", None, Some("name ASC"), 100).unwrap();
        assert_eq!(
            sql,
            "SELECT * FROM \"users\" WHERE TRUE ORDER BY name ASC LIMIT 100"
        );
    }

    #[test]
    fn build_select_with_both() {
        let sql = build_select("users", Some("age > 18"), Some("name DESC"), 50).unwrap();
        assert_eq!(
            sql,
            "SELECT * FROM \"users\" WHERE age > 18 ORDER BY name DESC LIMIT 50"
        );
    }

    #[test]
    fn build_select_invalid_sql() {
        let result = build_select("users", Some("INVALID SQL HERE @@@"), None, 100);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("SQL syntax error"));
    }

    #[test]
    fn build_select_with_cursor_no_where() {
        let cursor = CursorState {
            order_column: "id".to_string(),
            last_value: "42".to_string(),
            pk_type: "int8".to_string(),
        };
        let (sql, params) = build_select_with_cursor("users", None, None, &cursor, 100).unwrap();
        assert_eq!(
            sql,
            "SELECT * FROM \"users\" WHERE id > $1::int8 ORDER BY id LIMIT 100"
        );
        assert_eq!(params, vec!["42"]);
    }

    #[test]
    fn build_select_with_cursor_and_where() {
        let cursor = CursorState {
            order_column: "id".to_string(),
            last_value: "42".to_string(),
            pk_type: "int8".to_string(),
        };
        let (sql, params) = build_select_with_cursor(
            "users",
            Some("state = 'active'"),
            Some("id ASC"),
            &cursor,
            100,
        )
        .unwrap();
        assert_eq!(
            sql,
            "SELECT * FROM \"users\" WHERE (state = 'active') AND id > $1::int8 ORDER BY id ASC LIMIT 100"
        );
        assert_eq!(params, vec!["42"]);
    }

    #[test]
    fn build_select_empty_where_treated_as_true() {
        let sql = build_select("users", Some("   "), None, 100).unwrap();
        assert_eq!(
            sql,
            "SELECT * FROM \"users\" WHERE TRUE ORDER BY 1 LIMIT 100"
        );
    }

    #[test]
    fn build_select_cursor_no_cast_when_pk_type_empty() {
        let cursor = CursorState {
            order_column: "name".to_string(),
            last_value: "Alice".to_string(),
            pk_type: String::new(),
        };
        let (sql, _) = build_select_with_cursor("users", None, None, &cursor, 10).unwrap();
        assert_eq!(
            sql,
            "SELECT * FROM \"users\" WHERE name > $1 ORDER BY name LIMIT 10"
        );
    }

    #[test]
    fn build_sql_preview_basic() {
        let sql = build_sql_preview("products", None, None, 200).unwrap();
        assert_eq!(
            sql,
            "SELECT * FROM \"products\" WHERE TRUE ORDER BY 1 LIMIT 200;"
        );
    }

    #[test]
    fn build_sql_preview_with_where_and_order() {
        let sql = build_sql_preview(
            "products",
            Some("price > 499"),
            Some("created_at DESC"),
            200,
        )
        .unwrap();
        assert_eq!(
            sql,
            "SELECT * FROM \"products\" WHERE price > 499 ORDER BY created_at DESC LIMIT 200;"
        );
    }

    #[test]
    fn build_sql_preview_qualified_table() {
        let sql = build_sql_preview_qualified("devdb", "products", None, None, 200).unwrap();
        assert_eq!(
            sql,
            "SELECT * FROM \"devdb\".\"products\" WHERE TRUE ORDER BY 1 LIMIT 200;"
        );
    }

    #[test]
    fn build_sql_preview_qualified_table_escapes_identifiers() {
        let sql = build_sql_preview_qualified("dev\"db", "prod\"ucts", None, None, 200).unwrap();
        assert_eq!(
            sql,
            "SELECT * FROM \"dev\"\"db\".\"prod\"\"ucts\" WHERE TRUE ORDER BY 1 LIMIT 200;"
        );
    }

    #[test]
    fn build_select_qualified_table() {
        let sql = build_select_qualified("app", "users", None, None, 100).unwrap();
        assert_eq!(
            sql,
            "SELECT * FROM \"app\".\"users\" WHERE TRUE ORDER BY 1 LIMIT 100"
        );
    }

    #[test]
    fn build_select_with_cursor_qualified_table() {
        let cursor = CursorState {
            order_column: "id".to_string(),
            last_value: "42".to_string(),
            pk_type: "int8".to_string(),
        };
        let (sql, params) = build_select_with_cursor_qualified(
            "app",
            "users",
            Some("state = 'active'"),
            Some("id ASC"),
            &cursor,
            100,
        )
        .unwrap();
        assert_eq!(
            sql,
            "SELECT * FROM \"app\".\"users\" WHERE (state = 'active') AND id > $1::int8 ORDER BY id ASC LIMIT 100"
        );
        assert_eq!(params, vec!["42"]);
    }

    #[test]
    fn build_count_qualified_table() {
        let sql = build_count_qualified("app", "users", Some("state = 'active'")).unwrap();
        assert_eq!(
            sql,
            "SELECT COUNT(*) FROM \"app\".\"users\" WHERE state = 'active'"
        );
    }

    #[test]
    fn build_sql_preview_invalid_where() {
        let result = build_sql_preview("products", Some("INVALID @@@"), None, 200);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("SQL syntax error"));
    }

    #[test]
    fn editable_select_accepts_single_table_star() {
        assert!(is_editable_single_table_select("SELECT * FROM public.users").unwrap());
    }

    #[test]
    fn editable_select_target_extracts_schema_and_table() {
        let target = editable_single_table_select_target("SELECT * FROM public.users")
            .unwrap()
            .unwrap();
        assert_eq!(target.schema.as_deref(), Some("public"));
        assert_eq!(target.table, "users");
    }

    #[test]
    fn editable_select_target_defaults_schema_when_unqualified() {
        let target = editable_single_table_select_target("SELECT * FROM users")
            .unwrap()
            .unwrap();
        assert_eq!(target.schema.as_deref(), None);
        assert_eq!(target.table, "users");
    }

    #[test]
    fn build_postgres_insert_from_json_keys_only_inserts_present_columns() {
        let columns = vec!["name".to_string(), "price".to_string()];
        let sql = build_postgres_insert_from_json_keys("public", "products", &columns).unwrap();
        assert_eq!(
            sql,
            "INSERT INTO \"public\".\"products\" (\"name\", \"price\") SELECT tableleaf_row.\"name\", tableleaf_row.\"price\" FROM json_populate_record(NULL::\"public\".\"products\", $1::json) AS tableleaf_row"
        );
    }

    #[test]
    fn build_postgres_insert_from_json_keys_rejects_empty_columns() {
        let err = build_postgres_insert_from_json_keys("public", "products", &[]).unwrap_err();
        assert!(err.contains("No columns"));
    }

    #[test]
    fn build_postgres_update_from_json_uses_quoted_set_columns_and_text_key() {
        let sql = build_postgres_update_from_json(
            "public",
            "users",
            "id",
            &["name".to_string(), "price".to_string()],
        )
        .unwrap();
        assert_eq!(
            sql,
            "UPDATE \"public\".\"users\" SET \"name\" = tableleaf_patch.\"name\", \"price\" = tableleaf_patch.\"price\" FROM json_populate_record(NULL::\"public\".\"users\", $1::json) AS tableleaf_patch WHERE \"public\".\"users\".\"id\"::text = $2"
        );
    }

    #[test]
    fn editable_select_accepts_quoted_schema_with_filters() {
        assert!(is_editable_single_table_select(
            "SELECT * FROM \"app\".\"users\" WHERE id > 1 ORDER BY id DESC LIMIT 50"
        )
        .unwrap());
    }

    #[test]
    fn editable_select_rejects_join() {
        assert!(!is_editable_single_table_select(
            "SELECT * FROM users JOIN orders ON users.id = orders.user_id"
        )
        .unwrap());
    }

    #[test]
    fn editable_select_rejects_projection() {
        assert!(!is_editable_single_table_select("SELECT id, name FROM users").unwrap());
    }

    #[test]
    fn editable_select_rejects_cte_and_subquery() {
        assert!(!is_editable_single_table_select(
            "WITH active AS (SELECT * FROM users) SELECT * FROM active"
        )
        .unwrap());
        assert!(!is_editable_single_table_select("SELECT * FROM (SELECT * FROM users) u").unwrap());
    }

    #[test]
    fn editable_select_rejects_top_level_extras() {
        assert!(!is_editable_single_table_select("SELECT * FROM users OFFSET 10").unwrap());
        assert!(!is_editable_single_table_select("SELECT * FROM users FOR UPDATE").unwrap());
    }
}
