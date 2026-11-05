use serde::{Deserialize, Serialize};

const SQL_KEYWORDS: &[&str] = &[
    "SELECT",
    "FROM",
    "WHERE",
    "JOIN",
    "LEFT",
    "RIGHT",
    "INNER",
    "OUTER",
    "ON",
    "GROUP",
    "BY",
    "ORDER",
    "HAVING",
    "LIMIT",
    "OFFSET",
    "INSERT",
    "UPDATE",
    "DELETE",
    "CREATE",
    "DROP",
    "ALTER",
    "TABLE",
    "TRIGGER",
    "FUNCTION",
    "PROCEDURE",
    "RETURNS",
    "BEGIN",
    "END",
    "EXECUTE",
    "BEFORE",
    "AFTER",
    "INSTEAD",
    "EACH",
    "ROW",
    "STATEMENT",
    "WHEN",
    "LANGUAGE",
    "PLPGSQL",
    "INTO",
    "VALUES",
    "SET",
    "DISTINCT",
    "AS",
    "AND",
    "OR",
    "NOT",
    "NULL",
    "IS",
    "IN",
    "BETWEEN",
    "LIKE",
    "ASC",
    "DESC",
];

const SQLITE_SQL_KEYWORDS: &[&str] = &[
    "AUTOINCREMENT",
    "INTEGER",
    "BLOB",
    "BOOLEAN",
    "WITHOUT",
    "ROWID",
    "VACUUM",
    "PRAGMA",
    "JSON_EXTRACT",
    "JSON_SET",
    "STRFTIME",
    "IFNULL",
];

#[derive(Debug, Clone, Deserialize)]
pub struct CompletionRequest {
    /// SQL dialect — currently only "postgresql" / "postgres" is supported.
    #[serde(default)]
    pub dialect: String,
    pub query: String,
    pub cursor_pos: usize,
    #[serde(default)]
    pub catalog: CatalogSnapshot,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CatalogSnapshot {
    #[serde(default)]
    pub current_schema: Option<String>,
    #[serde(default)]
    pub current_table: Option<String>,
    #[serde(default)]
    pub schemas: Vec<SchemaInfo>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SchemaInfo {
    pub name: String,
    #[serde(default)]
    pub tables: Vec<TableInfo>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TableInfo {
    pub name: String,
    #[serde(default)]
    pub columns: Vec<ColumnInfo>,
    #[serde(default)]
    pub indexes: Vec<IndexInfo>,
    #[serde(default)]
    pub foreign_keys: Vec<ForeignKeyInfo>,
    #[serde(default)]
    pub comment: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ColumnInfo {
    pub name: String,
    #[serde(default)]
    pub data_type: Option<String>,
    #[serde(default)]
    pub nullable: Option<bool>,
    #[serde(default)]
    pub is_primary_key: bool,
    #[serde(default)]
    pub comment: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IndexInfo {
    pub name: String,
    #[serde(default)]
    pub columns: Vec<String>,
    #[serde(default)]
    pub is_unique: bool,
    #[serde(default)]
    pub is_primary: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ForeignKeyInfo {
    pub name: String,
    #[serde(default)]
    pub columns: Vec<String>,
    #[serde(default)]
    pub foreign_schema: String,
    #[serde(default)]
    pub foreign_table: String,
    #[serde(default)]
    pub foreign_columns: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CompletionResponse {
    pub items: Vec<CompletionItem>,
    pub replacement_range: (usize, usize),
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CompletionItem {
    pub label: String,
    pub kind: CompletionKind,
    pub detail: Option<String>,
    pub insert_text: String,
    pub score: i32,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompletionKind {
    Keyword,
    Table,
    Column,
}

#[derive(Debug, Clone)]
struct DerivedRelation {
    name: String,
    columns: Vec<DerivedColumn>,
}

#[derive(Debug, Clone)]
struct DerivedColumn {
    name: String,
}

#[must_use]
pub fn complete_sql(request: &CompletionRequest) -> CompletionResponse {
    let pos = clamp_to_char_boundary(&request.query, request.cursor_pos);
    if cursor_is_in_comment_or_string(&request.query[..pos]) {
        return CompletionResponse {
            items: Vec::new(),
            replacement_range: (pos, pos),
        };
    }
    let (start, partial) = partial_identifier(&request.query[..pos]);
    if let Some(qualifier) = qualifier_before_dot(&request.query[..start]) {
        let schema_tables = table_items_for_schema(&request.catalog, qualifier, partial);
        if !schema_tables.is_empty() || schema_exists(&request.catalog, qualifier) {
            return CompletionResponse {
                items: schema_tables,
                replacement_range: (start, pos),
            };
        }

        return CompletionResponse {
            items: column_items_for_table_qualifier(
                &request.catalog,
                &request.query,
                qualifier,
                partial,
            ),
            replacement_range: (start, pos),
        };
    }

    if is_table_position(&request.query[..start]) {
        return CompletionResponse {
            items: table_items(&request.catalog, &request.query[..start], partial),
            replacement_range: (start, pos),
        };
    }

    let column_items =
        column_items_for_referenced_tables(&request.catalog, &request.query, partial);

    let upper = partial.to_ascii_uppercase();
    let mut keyword_items = Vec::new();
    for keyword in sql_keywords_for_dialect(&request.dialect) {
        if keyword.starts_with(&upper) {
            keyword_items.push(CompletionItem {
                label: (*keyword).to_string(),
                kind: CompletionKind::Keyword,
                detail: Some("SQL keyword".to_string()),
                insert_text: (*keyword).to_string(),
                score: 100,
            });
        }
    }

    // Merge: columns (score 70) + keywords (score 100), sorted, deduped
    let mut items = column_items;
    items.extend(keyword_items);
    items.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.label.cmp(&b.label)));
    items.dedup_by(|a, b| a.label == b.label);

    CompletionResponse {
        items,
        replacement_range: (start, pos),
    }
}

fn sql_keywords_for_dialect(dialect: &str) -> impl Iterator<Item = &'static str> {
    let extra = if dialect.eq_ignore_ascii_case("sqlite") {
        SQLITE_SQL_KEYWORDS
    } else {
        &[]
    };
    SQL_KEYWORDS.iter().copied().chain(extra.iter().copied())
}

fn clamp_to_char_boundary(text: &str, cursor_pos: usize) -> usize {
    let mut pos = cursor_pos.min(text.len());
    while pos > 0 && !text.is_char_boundary(pos) {
        pos -= 1;
    }
    pos
}

fn partial_identifier(text_before_cursor: &str) -> (usize, &str) {
    let mut start = text_before_cursor.len();
    for (idx, ch) in text_before_cursor.char_indices().rev() {
        if is_identifier_char(ch) {
            start = idx;
        } else {
            break;
        }
    }
    (start, &text_before_cursor[start..])
}

fn is_identifier_char(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphanumeric()
}

fn qualifier_before_dot(text_before_partial: &str) -> Option<&str> {
    let before_dot = text_before_partial.strip_suffix('.')?;
    let (start, qualifier) = partial_identifier(before_dot);
    if qualifier.is_empty() {
        None
    } else {
        Some(&before_dot[start..])
    }
}

fn is_table_position(text_before_partial: &str) -> bool {
    let trimmed = text_before_partial.trim_end();
    // Comma after table list in FROM/JOIN: "FROM t1, " → suggest next table
    if trimmed.ends_with(',') {
        let before_comma = trimmed.trim_end_matches(',').trim_end();
        return is_inside_from_clause(before_comma);
    }
    matches!(
        previous_word(text_before_partial).as_deref(),
        Some("FROM") | Some("JOIN") | Some("UPDATE") | Some("INTO")
    )
}

/// Returns true when `text` appears inside a FROM/JOIN table list (no intervening
/// WHERE / GROUP / ORDER / ON / HAVING after the last FROM or JOIN keyword).
fn is_inside_from_clause(text: &str) -> bool {
    let upper = text.to_ascii_uppercase();
    let clause_kws = ["FROM", "JOIN"];
    let stop_kws = ["WHERE", "GROUP", "ORDER", "ON", "HAVING", "SET"];
    // Find the last FROM/JOIN position
    let last_from = clause_kws.iter().filter_map(|kw| upper.rfind(kw)).max();
    let Some(from_pos) = last_from else {
        return false;
    };
    // No stop keyword between that position and end
    !stop_kws.iter().any(|kw| upper[from_pos..].contains(kw))
}

fn previous_word(text: &str) -> Option<String> {
    let trimmed = text.trim_end();
    let (_, word) = partial_identifier(trimmed);
    if word.is_empty() {
        None
    } else {
        Some(word.to_ascii_uppercase())
    }
}

fn table_items(
    catalog: &CatalogSnapshot,
    query_before_cursor: &str,
    partial: &str,
) -> Vec<CompletionItem> {
    let mut items = Vec::new();
    for cte_name in cte_names(query_before_cursor) {
        if starts_with_ignore_ascii_case(&cte_name, partial) {
            items.push(CompletionItem {
                label: cte_name.clone(),
                kind: CompletionKind::Table,
                detail: Some("CTE".to_string()),
                insert_text: cte_name,
                score: 95,
            });
        }
    }
    for schema in &catalog.schemas {
        for table in &schema.tables {
            if starts_with_ignore_ascii_case(&table.name, partial) {
                items.push(CompletionItem {
                    label: table.name.clone(),
                    kind: CompletionKind::Table,
                    detail: Some(format!("{}.{}", schema.name, table.name)),
                    insert_text: table.name.clone(),
                    score: 80,
                });
            }
        }
    }
    items.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.label.cmp(&b.label)));
    items
}

fn table_items_for_schema(
    catalog: &CatalogSnapshot,
    schema_name: &str,
    partial: &str,
) -> Vec<CompletionItem> {
    let mut items = Vec::new();
    for schema in &catalog.schemas {
        if !schema.name.eq_ignore_ascii_case(schema_name) {
            continue;
        }
        for table in &schema.tables {
            if starts_with_ignore_ascii_case(&table.name, partial) {
                items.push(CompletionItem {
                    label: table.name.clone(),
                    kind: CompletionKind::Table,
                    detail: Some(format!("{}.{}", schema.name, table.name)),
                    insert_text: table.name.clone(),
                    score: 90,
                });
            }
        }
    }
    items.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.label.cmp(&b.label)));
    items
}

fn schema_exists(catalog: &CatalogSnapshot, schema_name: &str) -> bool {
    catalog
        .schemas
        .iter()
        .any(|schema| schema.name.eq_ignore_ascii_case(schema_name))
}

fn column_items_for_table_qualifier(
    catalog: &CatalogSnapshot,
    query: &str,
    qualifier: &str,
    partial: &str,
) -> Vec<CompletionItem> {
    let mut items = Vec::new();
    let derived = derived_relations(catalog, query);
    for relation in &derived {
        if relation.name.eq_ignore_ascii_case(qualifier) {
            append_derived_column_items(&mut items, relation, partial, None, 95);
            items.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.label.cmp(&b.label)));
            items.dedup_by(|a, b| a.label == b.label);
            return items;
        }
    }

    for (alias, relation_name) in derived_aliases(query).into_iter().rev() {
        if alias.eq_ignore_ascii_case(qualifier) {
            if let Some(relation) = derived
                .iter()
                .find(|relation| relation.name.eq_ignore_ascii_case(&relation_name))
            {
                let prefix =
                    (!relation.name.eq_ignore_ascii_case(&alias)).then_some(alias.as_str());
                append_derived_column_items(&mut items, relation, partial, prefix, 95);
                items.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.label.cmp(&b.label)));
                items.dedup_by(|a, b| a.label == b.label);
                return items;
            }
        }
    }

    // Check alias map first so "u." resolves to "users" columns
    let alias_map = build_alias_map(catalog, query);
    for (alias, (schema, table)) in alias_map.iter().rev() {
        if alias.eq_ignore_ascii_case(qualifier) {
            let (detail_prefix, score) = if table.name.eq_ignore_ascii_case(alias) {
                (None, 90)
            } else {
                (Some(alias.as_str()), 95)
            };
            append_column_items_with_context(
                &mut items,
                schema,
                table,
                partial,
                detail_prefix,
                score,
            );
            items.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.label.cmp(&b.label)));
            items.dedup_by(|a, b| a.label == b.label);
            return items;
        }
    }
    // Fallback: scan all tables by real name (catches schema.table qualifiers)
    if items.is_empty() {
        for (schema, table) in all_tables(catalog) {
            if !table.name.eq_ignore_ascii_case(qualifier) {
                continue;
            }
            append_column_items_with_context(&mut items, schema, table, partial, None, 90);
        }
    }
    items.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.label.cmp(&b.label)));
    items.dedup_by(|a, b| a.label == b.label);
    items
}

fn column_items_for_referenced_tables(
    catalog: &CatalogSnapshot,
    query: &str,
    partial: &str,
) -> Vec<CompletionItem> {
    let mut items = Vec::new();
    let mut tables = referenced_tables(catalog, query);
    if tables.is_empty() {
        if let Some(table) = current_table(catalog) {
            tables.push(table);
        }
    }

    for (schema, table) in tables {
        append_column_items_with_context(&mut items, schema, table, partial, None, 70);
    }
    items.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.label.cmp(&b.label)));
    items.dedup_by(|a, b| a.label == b.label);
    items
}

fn current_table(catalog: &CatalogSnapshot) -> Option<(&SchemaInfo, &TableInfo)> {
    let table_name = catalog.current_table.as_deref()?;
    for schema in &catalog.schemas {
        if catalog
            .current_schema
            .as_deref()
            .is_some_and(|name| !schema.name.eq_ignore_ascii_case(name))
        {
            continue;
        }
        for table in &schema.tables {
            if table.name.eq_ignore_ascii_case(table_name) {
                return Some((schema, table));
            }
        }
    }
    None
}

fn append_column_items_with_context(
    items: &mut Vec<CompletionItem>,
    _schema: &SchemaInfo,
    table: &TableInfo,
    partial: &str,
    _detail_prefix: Option<&str>,
    score: i32,
) {
    for column in &table.columns {
        if starts_with_ignore_ascii_case(&column.name, partial) {
            items.push(CompletionItem {
                label: column.name.clone(),
                kind: CompletionKind::Column,
                detail: column.data_type.clone(),
                insert_text: column.name.clone(),
                score,
            });
        }
    }
}

fn append_derived_column_items(
    items: &mut Vec<CompletionItem>,
    relation: &DerivedRelation,
    partial: &str,
    detail_prefix: Option<&str>,
    score: i32,
) {
    for column in &relation.columns {
        if starts_with_ignore_ascii_case(&column.name, partial) {
            let detail = match detail_prefix {
                Some(prefix) => format!("{} -> {}.{}", prefix, relation.name, column.name),
                None => format!("{}.{}", relation.name, column.name),
            };
            items.push(CompletionItem {
                label: column.name.clone(),
                kind: CompletionKind::Column,
                detail: Some(detail),
                insert_text: column.name.clone(),
                score,
            });
        }
    }
}

fn all_tables(catalog: &CatalogSnapshot) -> Vec<(&SchemaInfo, &TableInfo)> {
    catalog
        .schemas
        .iter()
        .flat_map(|schema| schema.tables.iter().map(move |table| (schema, table)))
        .collect()
}

/// Build a map from alias/name → table for every table referenced in the query.
/// Handles: `FROM users`, `FROM users u`, `FROM users AS u`, `JOIN schema.t AS t2`.
fn build_alias_map<'a>(
    catalog: &'a CatalogSnapshot,
    query: &str,
) -> Vec<(String, (&'a SchemaInfo, &'a TableInfo))> {
    let tokens = sql_tokens(query);
    let mut result = Vec::new();
    let sql_keywords_upper: std::collections::HashSet<&str> = [
        "SELECT", "FROM", "WHERE", "JOIN", "LEFT", "RIGHT", "INNER", "OUTER", "ON", "GROUP", "BY",
        "ORDER", "HAVING", "LIMIT", "OFFSET", "AS", "AND", "OR", "NOT", "NULL", "IS", "IN",
        "BETWEEN", "LIKE", "ASC", "DESC", "SET", "DISTINCT", "INTO", "VALUES", "UPDATE", "DELETE",
        "CREATE", "DROP", "ALTER",
    ]
    .into();

    let mut i = 0;
    while i < tokens.len() {
        let upper = tokens[i].to_ascii_uppercase();
        if matches!(upper.as_str(), "FROM" | "JOIN") && i + 1 < tokens.len() {
            let table_ref = &tokens[i + 1];
            if let Some(entry) = find_table(catalog, table_ref) {
                let table_name = entry.1.name.clone();
                // Register the real table name itself
                result.push((table_name.to_ascii_lowercase(), entry));
                result.push((table_name.clone(), entry));

                // Check for optional alias: FROM users [AS] alias
                let alias_idx = if i + 2 < tokens.len()
                    && tokens[i + 2].to_ascii_uppercase() == "AS"
                    && i + 3 < tokens.len()
                {
                    i + 3
                } else if i + 2 < tokens.len()
                    && !sql_keywords_upper.contains(tokens[i + 2].to_ascii_uppercase().as_str())
                {
                    i + 2
                } else {
                    usize::MAX
                };

                if alias_idx < tokens.len() {
                    let alias = tokens[alias_idx].to_string();
                    if !sql_keywords_upper.contains(alias.to_ascii_uppercase().as_str()) {
                        result.push((alias, entry));
                    }
                }
            }
        }
        i += 1;
    }
    result
}

fn referenced_tables<'a>(
    catalog: &'a CatalogSnapshot,
    query: &str,
) -> Vec<(&'a SchemaInfo, &'a TableInfo)> {
    build_alias_map(catalog, query)
        .into_iter()
        .map(|(_, entry)| entry)
        .collect::<Vec<_>>()
        // dedup by pointer identity (same table can appear under real name + alias)
        .into_iter()
        .fold(Vec::new(), |mut acc, entry| {
            if !acc.iter().any(|e: &(&SchemaInfo, &TableInfo)| {
                std::ptr::eq(e.1 as *const TableInfo, entry.1 as *const TableInfo)
            }) {
                acc.push(entry);
            }
            acc
        })
}

fn find_table<'a>(
    catalog: &'a CatalogSnapshot,
    table_ref: &str,
) -> Option<(&'a SchemaInfo, &'a TableInfo)> {
    let cleaned = table_ref
        .trim_matches(|ch: char| ch == ';' || ch == ',' || ch == ')' || ch == '(' || ch == '"');
    let (schema_name, table_name) = cleaned
        .rsplit_once('.')
        .map_or((None, cleaned), |(schema, table)| (Some(schema), table));

    for schema in &catalog.schemas {
        if schema_name.is_some_and(|name| !schema.name.eq_ignore_ascii_case(name)) {
            continue;
        }
        for table in &schema.tables {
            if table.name.eq_ignore_ascii_case(table_name) {
                return Some((schema, table));
            }
        }
    }
    None
}

fn cte_names(query: &str) -> Vec<String> {
    let trimmed = query.trim_start();
    if !trimmed.to_ascii_uppercase().starts_with("WITH ") {
        return Vec::new();
    }

    let tokens = sql_tokens(trimmed);
    let mut names = Vec::new();
    let mut index = 1;
    while index + 1 < tokens.len() {
        let name = tokens[index];
        if name.eq_ignore_ascii_case("RECURSIVE") {
            index += 1;
            continue;
        }
        if tokens[index + 1].eq_ignore_ascii_case("AS") {
            names.push(name.trim_matches('"').to_string());
            index += 2;
            continue;
        }
        index += 1;
    }
    names
}

fn derived_relations(catalog: &CatalogSnapshot, query: &str) -> Vec<DerivedRelation> {
    let mut relations = cte_relations(catalog, query);
    relations.extend(subquery_relations(catalog, query));
    relations
}

fn cte_relations(catalog: &CatalogSnapshot, query: &str) -> Vec<DerivedRelation> {
    let mut relations = Vec::new();
    let Some(with_start) = find_keyword(query, "WITH", 0) else {
        return relations;
    };
    let mut pos = with_start + "WITH".len();
    while pos < query.len() {
        pos = skip_space_and_commas(query, pos);
        if query[pos..].to_ascii_uppercase().starts_with("RECURSIVE") {
            pos += "RECURSIVE".len();
            continue;
        }
        let Some((name, after_name)) = read_identifier(query, pos) else {
            break;
        };
        let Some(as_pos) = find_keyword(query, "AS", after_name) else {
            break;
        };
        let Some(open_rel) = query[as_pos..].find('(') else {
            break;
        };
        let open = as_pos + open_rel;
        let Some(close) = matching_paren(query, open) else {
            break;
        };
        let inner = &query[open + 1..close];
        relations.push(DerivedRelation {
            name,
            columns: infer_select_columns(catalog, inner),
        });
        pos = close + 1;
        let rest = query[pos..].trim_start();
        if !rest.starts_with(',') {
            break;
        }
    }
    relations
}

fn subquery_relations(catalog: &CatalogSnapshot, query: &str) -> Vec<DerivedRelation> {
    let mut relations = Vec::new();
    let bytes = query.as_bytes();
    let mut pos = 0;
    while pos < query.len() {
        let Some(keyword_pos) = find_next_from_join(query, pos) else {
            break;
        };
        let after_keyword = skip_whitespace(query, keyword_pos.1);
        if bytes.get(after_keyword) == Some(&b'(') {
            if let Some(close) = matching_paren(query, after_keyword) {
                let mut alias_pos = skip_whitespace(query, close + 1);
                if query[alias_pos..].to_ascii_uppercase().starts_with("AS ") {
                    alias_pos = skip_whitespace(query, alias_pos + 2);
                }
                if let Some((alias, _)) = read_identifier(query, alias_pos) {
                    let inner = &query[after_keyword + 1..close];
                    relations.push(DerivedRelation {
                        name: alias,
                        columns: infer_select_columns(catalog, inner),
                    });
                }
                pos = close + 1;
                continue;
            }
        }
        pos = keyword_pos.1;
    }
    relations
}

fn derived_aliases(query: &str) -> Vec<(String, String)> {
    let relation_names: Vec<String> = cte_names(query)
        .into_iter()
        .chain(
            subquery_relations(&CatalogSnapshot::default(), query)
                .into_iter()
                .map(|relation| relation.name),
        )
        .collect();
    let tokens = sql_tokens(query);
    let mut aliases = Vec::new();
    let mut i = 0;
    while i + 1 < tokens.len() {
        let upper = tokens[i].to_ascii_uppercase();
        if matches!(upper.as_str(), "FROM" | "JOIN") {
            let relation = tokens[i + 1].trim_matches('"');
            if relation_names
                .iter()
                .any(|name| name.eq_ignore_ascii_case(relation))
            {
                aliases.push((relation.to_string(), relation.to_string()));
                let alias_idx = if i + 2 < tokens.len()
                    && tokens[i + 2].eq_ignore_ascii_case("AS")
                    && i + 3 < tokens.len()
                {
                    Some(i + 3)
                } else if i + 2 < tokens.len() && !is_clause_keyword(tokens[i + 2]) {
                    Some(i + 2)
                } else {
                    None
                };
                if let Some(alias_idx) = alias_idx {
                    aliases.push((
                        tokens[alias_idx].trim_matches('"').to_string(),
                        relation.to_string(),
                    ));
                }
            }
        }
        i += 1;
    }
    aliases
}

fn infer_select_columns(catalog: &CatalogSnapshot, query: &str) -> Vec<DerivedColumn> {
    let upper = query.to_ascii_uppercase();
    let Some(select_pos) = upper.find("SELECT") else {
        return Vec::new();
    };
    let from_pos = find_keyword(query, "FROM", select_pos + "SELECT".len());
    let select_list_end = from_pos.unwrap_or(query.len());
    let select_list = &query[select_pos + "SELECT".len()..select_list_end];
    let mut columns = Vec::new();
    for expr in split_top_level_commas(select_list) {
        let expr = expr.trim();
        if expr == "*" {
            if let Some(from_pos) = from_pos {
                if let Some((table_ref, _)) =
                    read_identifier_path(query, skip_whitespace(query, from_pos + "FROM".len()))
                {
                    if let Some((_, table)) = find_table(catalog, &table_ref) {
                        columns.extend(table.columns.iter().map(|column| DerivedColumn {
                            name: column.name.clone(),
                        }));
                    }
                }
            }
            continue;
        }
        if let Some(alias) = alias_for_select_expr(expr) {
            columns.push(DerivedColumn { name: alias });
        } else if let Some(name) = trailing_identifier(expr) {
            columns.push(DerivedColumn { name });
        }
    }
    columns
}

fn alias_for_select_expr(expr: &str) -> Option<String> {
    let tokens = sql_tokens(expr);
    for window in tokens.windows(2) {
        if window[0].eq_ignore_ascii_case("AS") {
            return Some(window[1].trim_matches('"').to_string());
        }
    }
    None
}

fn trailing_identifier(expr: &str) -> Option<String> {
    let cleaned = expr.trim().trim_matches('"');
    if cleaned.contains('(') || cleaned.contains(')') {
        return None;
    }
    let tail = cleaned.rsplit('.').next()?.trim_matches('"');
    (!tail.is_empty() && tail.chars().all(is_identifier_char)).then(|| tail.to_string())
}

fn split_top_level_commas(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut depth = 0usize;
    for (idx, ch) in text.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(&text[start..idx]);
                start = idx + 1;
            }
            _ => {}
        }
    }
    parts.push(&text[start..]);
    parts
}

fn find_keyword(text: &str, keyword: &str, start: usize) -> Option<usize> {
    let upper = text.to_ascii_uppercase();
    let keyword = keyword.to_ascii_uppercase();
    upper[start..].find(&keyword).map(|pos| pos + start)
}

fn find_next_from_join(text: &str, start: usize) -> Option<(usize, usize)> {
    let from = find_keyword(text, "FROM", start).map(|pos| (pos, pos + "FROM".len()));
    let join = find_keyword(text, "JOIN", start).map(|pos| (pos, pos + "JOIN".len()));
    match (from, join) {
        (Some(from), Some(join)) => Some(if from.0 <= join.0 { from } else { join }),
        (Some(from), None) => Some(from),
        (None, Some(join)) => Some(join),
        (None, None) => None,
    }
}

fn matching_paren(text: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (idx, ch) in text[open..].char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(open + idx);
                }
            }
            _ => {}
        }
    }
    None
}

fn skip_space_and_commas(text: &str, mut pos: usize) -> usize {
    while pos < text.len() && matches!(text.as_bytes()[pos], b' ' | b'\n' | b'\t' | b'\r' | b',') {
        pos += 1;
    }
    pos
}

fn skip_whitespace(text: &str, mut pos: usize) -> usize {
    while pos < text.len() && text.as_bytes()[pos].is_ascii_whitespace() {
        pos += 1;
    }
    pos
}

fn read_identifier(text: &str, pos: usize) -> Option<(String, usize)> {
    let mut end = pos;
    let bytes = text.as_bytes();
    if bytes.get(pos) == Some(&b'"') {
        end += 1;
        while end < text.len() && bytes[end] != b'"' {
            end += 1;
        }
        return (end < text.len()).then(|| (text[pos + 1..end].to_string(), end + 1));
    }
    while end < text.len() {
        let ch = text[end..].chars().next()?;
        if is_identifier_char(ch) {
            end += ch.len_utf8();
        } else {
            break;
        }
    }
    (end > pos).then(|| (text[pos..end].to_string(), end))
}

fn read_identifier_path(text: &str, pos: usize) -> Option<(String, usize)> {
    let mut end = pos;
    while end < text.len() {
        let ch = text[end..].chars().next()?;
        if is_identifier_char(ch) || ch == '.' || ch == '"' {
            end += ch.len_utf8();
        } else {
            break;
        }
    }
    (end > pos).then(|| (text[pos..end].to_string(), end))
}

fn is_clause_keyword(token: &str) -> bool {
    matches!(
        token.to_ascii_uppercase().as_str(),
        "WHERE"
            | "JOIN"
            | "LEFT"
            | "RIGHT"
            | "INNER"
            | "OUTER"
            | "FULL"
            | "CROSS"
            | "ON"
            | "GROUP"
            | "ORDER"
            | "HAVING"
            | "LIMIT"
            | "OFFSET"
            | "UNION"
            | "EXCEPT"
            | "INTERSECT"
            | "AS"
    )
}

fn cursor_is_in_comment_or_string(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;

    while i < bytes.len() {
        let current = bytes[i];
        let next = bytes.get(i + 1).copied();

        if in_line_comment {
            if current == b'\n' {
                in_line_comment = false;
            }
            i += 1;
            continue;
        }

        if in_block_comment {
            if current == b'*' && next == Some(b'/') {
                in_block_comment = false;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }

        if in_single {
            if current == b'\'' && next == Some(b'\'') {
                i += 2;
            } else if current == b'\'' {
                in_single = false;
                i += 1;
            } else {
                i += 1;
            }
            continue;
        }

        if in_double {
            if current == b'"' && next == Some(b'"') {
                i += 2;
            } else if current == b'"' {
                in_double = false;
                i += 1;
            } else {
                i += 1;
            }
            continue;
        }

        match (current, next) {
            (b'-', Some(b'-')) => {
                in_line_comment = true;
                i += 2;
            }
            (b'/', Some(b'*')) => {
                in_block_comment = true;
                i += 2;
            }
            (b'\'', _) => {
                in_single = true;
                i += 1;
            }
            (b'"', _) => {
                in_double = true;
                i += 1;
            }
            _ => i += 1,
        }
    }

    in_single || in_double || in_line_comment || in_block_comment
}

fn sql_tokens(query: &str) -> Vec<&str> {
    query
        .split(|ch: char| ch.is_whitespace() || ch == ',' || ch == '(' || ch == ')')
        .filter(|token| !token.is_empty())
        .collect()
}

fn starts_with_ignore_ascii_case(value: &str, partial: &str) -> bool {
    partial.is_empty()
        || value
            .get(..partial.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(partial))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_col(name: &str, data_type: Option<&str>) -> ColumnInfo {
        ColumnInfo {
            name: name.to_string(),
            data_type: data_type.map(str::to_string),
            nullable: None,
            is_primary_key: false,
            comment: None,
        }
    }

    fn test_table(name: &str, columns: Vec<ColumnInfo>) -> TableInfo {
        TableInfo {
            name: name.to_string(),
            columns,
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            comment: None,
        }
    }

    fn request(query: &str, cursor_pos: usize) -> CompletionRequest {
        CompletionRequest {
            dialect: "postgresql".to_string(),
            query: query.to_string(),
            cursor_pos,
            catalog: CatalogSnapshot::default(),
        }
    }

    fn catalog_request(query: &str) -> CompletionRequest {
        CompletionRequest {
            dialect: "postgresql".to_string(),
            query: query.to_string(),
            cursor_pos: query.len(),
            catalog: CatalogSnapshot {
                current_schema: None,
                current_table: None,
                schemas: vec![SchemaInfo {
                    name: "public".to_string(),
                    tables: vec![test_table(
                        "users",
                        vec![
                            test_col("id", Some("int8")),
                            test_col("email", Some("text")),
                        ],
                    )],
                }],
            },
        }
    }

    #[test]
    fn completes_keyword_prefix_with_replacement_range() {
        let response = complete_sql(&request("SEL", 3));

        assert_eq!(response.replacement_range, (0, 3));
        assert!(response.items.iter().any(|item| {
            item.label == "SELECT"
                && item.kind == CompletionKind::Keyword
                && item.insert_text == "SELECT"
        }));
    }

    #[test]
    fn completes_postgres_ddl_trigger_keyword() {
        let response = complete_sql(&request("CREATE TR", "CREATE TR".len()));

        assert_eq!(
            response.replacement_range,
            ("CREATE ".len(), "CREATE TR".len())
        );
        assert!(response.items.iter().any(|item| {
            item.label == "TRIGGER"
                && item.kind == CompletionKind::Keyword
                && item.insert_text == "TRIGGER"
        }));
    }

    #[test]
    fn completes_tables_after_schema_qualifier() {
        let query = "SELECT * FROM public.";

        let response = complete_sql(&catalog_request(query));

        assert_eq!(response.replacement_range, (query.len(), query.len()));
        assert!(response.items.iter().any(|item| {
            item.label == "users"
                && item.kind == CompletionKind::Table
                && item.detail.as_deref() == Some("public.users")
        }));
    }

    #[test]
    fn completes_tables_after_from_keyword() {
        let query = "SELECT * FROM us";

        let response = complete_sql(&catalog_request(query));

        assert_eq!(response.replacement_range, (query.len() - 2, query.len()));
        assert!(response.items.iter().any(|item| {
            item.label == "users"
                && item.kind == CompletionKind::Table
                && item.detail.as_deref() == Some("public.users")
        }));
    }

    #[test]
    fn completes_columns_for_referenced_table() {
        let query = "SELECT em FROM public.users";

        let response = complete_sql(&CompletionRequest {
            cursor_pos: "SELECT em".len(),
            ..catalog_request(query)
        });

        assert_eq!(
            response.replacement_range,
            ("SELECT ".len(), "SELECT em".len())
        );
        assert!(response.items.iter().any(|item| {
            item.label == "email"
                && item.kind == CompletionKind::Column
                && item.detail.as_deref() == Some("text")
        }));
    }

    #[test]
    fn completes_columns_after_table_qualifier() {
        let query = "SELECT users. FROM public.users";

        let response = complete_sql(&CompletionRequest {
            cursor_pos: "SELECT users.".len(),
            ..catalog_request(query)
        });

        assert_eq!(
            response.replacement_range,
            ("SELECT users.".len(), "SELECT users.".len())
        );
        assert!(response.items.iter().any(|item| {
            item.label == "email"
                && item.kind == CompletionKind::Column
                && item.detail.as_deref() == Some("text")
        }));
    }

    #[test]
    fn completes_columns_from_current_table_context() {
        let query = "em";
        let mut request = catalog_request(query);
        request.catalog.current_schema = Some("public".to_string());
        request.catalog.current_table = Some("users".to_string());

        let response = complete_sql(&request);

        assert_eq!(response.replacement_range, (0, 2));
        assert!(response.items.iter().any(|item| {
            item.label == "email"
                && item.kind == CompletionKind::Column
                && item.detail.as_deref() == Some("text")
        }));
    }

    #[test]
    fn completes_columns_via_table_alias() {
        let query = "SELECT u. FROM users u";

        let response = complete_sql(&CompletionRequest {
            cursor_pos: "SELECT u.".len(),
            ..catalog_request(query)
        });

        assert!(
            !response.items.is_empty(),
            "should resolve alias 'u' to table 'users'"
        );
        assert!(response
            .items
            .iter()
            .any(|item| { item.label == "email" && item.kind == CompletionKind::Column }));
    }

    #[test]
    fn completes_columns_after_implicit_table_alias() {
        let query = "SELECT u. FROM public.users u";

        let response = complete_sql(&CompletionRequest {
            cursor_pos: "SELECT u.".len(),
            ..catalog_request(query)
        });

        assert!(response.items.iter().any(|item| {
            item.label == "email"
                && item.kind == CompletionKind::Column
                && item.detail.as_deref() == Some("text")
        }));
    }

    #[test]
    fn completes_columns_after_as_table_alias() {
        let query = "SELECT usr. FROM public.users AS usr";

        let response = complete_sql(&CompletionRequest {
            cursor_pos: "SELECT usr.".len(),
            ..catalog_request(query)
        });

        assert!(response.items.iter().any(|item| {
            item.label == "email"
                && item.kind == CompletionKind::Column
                && item.detail.as_deref() == Some("text")
        }));
    }

    #[test]
    fn ranks_alias_columns_above_unqualified_catalog_matches() {
        let query = "SELECT u.e FROM public.users u";

        let response = complete_sql(&CompletionRequest {
            cursor_pos: "SELECT u.e".len(),
            ..catalog_request(query)
        });

        let first = response
            .items
            .first()
            .expect("expected alias column suggestions");
        assert_eq!(first.label, "email");
        assert_eq!(first.kind, CompletionKind::Column);
        assert!(first.score >= 90);
    }

    #[test]
    fn completes_cte_name_after_from() {
        let query = "WITH recent_users AS (SELECT id, email FROM public.users) SELECT * FROM rec";

        let response = complete_sql(&CompletionRequest {
            cursor_pos: query.len(),
            ..catalog_request(query)
        });

        assert!(response.items.iter().any(|item| {
            item.label == "recent_users"
                && item.kind == CompletionKind::Table
                && item.detail.as_deref() == Some("CTE")
        }));
    }

    #[test]
    fn completes_cte_columns_after_cte_qualifier() {
        let query = "WITH recent_users AS (SELECT id, email FROM public.users) SELECT recent_users. FROM recent_users";

        let response = complete_sql(&CompletionRequest {
            cursor_pos:
                "WITH recent_users AS (SELECT id, email FROM public.users) SELECT recent_users."
                    .len(),
            ..catalog_request(query)
        });

        assert!(response.items.iter().any(|item| {
            item.label == "email"
                && item.kind == CompletionKind::Column
                && item.detail.as_deref() == Some("recent_users.email")
        }));
    }

    #[test]
    fn completes_cte_alias_columns() {
        let query = "WITH recent_users AS (SELECT id, email FROM public.users) SELECT r. FROM recent_users r";

        let response = complete_sql(&CompletionRequest {
            cursor_pos: "WITH recent_users AS (SELECT id, email FROM public.users) SELECT r.".len(),
            ..catalog_request(query)
        });

        assert!(response.items.iter().any(|item| {
            item.label == "email"
                && item.kind == CompletionKind::Column
                && item.detail.as_deref() == Some("r -> recent_users.email")
        }));
    }

    #[test]
    fn completes_columns_after_subquery_alias() {
        let query = "SELECT sq. FROM (SELECT id, email FROM public.users) sq";

        let response = complete_sql(&CompletionRequest {
            cursor_pos: "SELECT sq.".len(),
            ..catalog_request(query)
        });

        assert!(response.items.iter().any(|item| {
            item.label == "email"
                && item.kind == CompletionKind::Column
                && item.detail.as_deref() == Some("sq.email")
        }));
    }

    fn catalog_with_users_and_orders_request(query: &str) -> CompletionRequest {
        CompletionRequest {
            dialect: "postgresql".to_string(),
            query: query.to_string(),
            cursor_pos: query.len(),
            catalog: CatalogSnapshot {
                current_schema: Some("public".to_string()),
                current_table: None,
                schemas: vec![SchemaInfo {
                    name: "public".to_string(),
                    tables: vec![
                        test_table(
                            "users",
                            vec![
                                test_col("id", Some("int8")),
                                test_col("email", Some("text")),
                            ],
                        ),
                        test_table(
                            "orders",
                            vec![
                                test_col("id", Some("int8")),
                                test_col("user_id", Some("int8")),
                            ],
                        ),
                    ],
                }],
            },
        }
    }

    #[test]
    fn alias_shadows_table_name_for_dotted_completion() {
        let query =
            "SELECT users. FROM public.users u JOIN public.orders users ON users.user_id = u.id";

        let response = complete_sql(&CompletionRequest {
            cursor_pos: "SELECT users.".len(),
            ..catalog_with_users_and_orders_request(query)
        });

        assert!(response.items.iter().any(|item| item.label == "user_id"));
        assert!(!response.items.iter().any(|item| item.label == "email"));
    }

    #[test]
    fn cte_name_shadows_catalog_table_name() {
        let query =
            "WITH users AS (SELECT email AS cte_email FROM public.users) SELECT users. FROM users";

        let response = complete_sql(&CompletionRequest {
            cursor_pos: "WITH users AS (SELECT email AS cte_email FROM public.users) SELECT users."
                .len(),
            ..catalog_request(query)
        });

        assert!(response.items.iter().any(|item| item.label == "cte_email"));
        assert!(!response.items.iter().any(|item| item.label == "email"));
    }

    #[test]
    fn does_not_complete_inside_single_quoted_string() {
        let query = "SELECT 'SEL";

        let response = complete_sql(&request(query, query.len()));

        assert!(response.items.is_empty());
    }

    #[test]
    fn does_not_complete_inside_line_comment() {
        let query = "SELECT 1 -- SEL";

        let response = complete_sql(&request(query, query.len()));

        assert!(response.items.is_empty());
    }

    #[test]
    fn replacement_range_uses_utf8_byte_offsets_after_non_ascii() {
        let query = "SELECT 'é'; SEL";
        let cursor = query.len();

        let response = complete_sql(&request(query, cursor));

        assert_eq!(
            response.replacement_range,
            ("SELECT 'é'; ".len(), query.len())
        );
        assert!(response.items.iter().any(|item| item.label == "SELECT"));
    }

    #[test]
    fn completes_columns_via_as_alias() {
        let query = "SELECT usr. FROM users AS usr";

        let response = complete_sql(&CompletionRequest {
            cursor_pos: "SELECT usr.".len(),
            ..catalog_request(query)
        });

        assert!(
            !response.items.is_empty(),
            "should resolve AS alias 'usr' to table 'users'"
        );
        assert!(response.items.iter().any(|item| item.label == "id"));
    }

    #[test]
    fn keywords_appear_even_when_columns_match() {
        let query = "SELECT se FROM users";
        let mut req = catalog_request(query);
        req.cursor_pos = "SELECT se".len();
        // Add a column named "search_count" to make column branch non-empty
        req.catalog.schemas[0].tables[0]
            .columns
            .push(test_col("search_count", None));

        let response = complete_sql(&req);

        let has_keyword = response
            .items
            .iter()
            .any(|i| i.label == "SELECT" && i.kind == CompletionKind::Keyword);
        let has_column = response
            .items
            .iter()
            .any(|i| i.label == "search_count" && i.kind == CompletionKind::Column);
        assert!(
            has_keyword,
            "SELECT keyword should appear even when columns match 'se'"
        );
        assert!(has_column, "search_count column should appear");
    }

    #[test]
    fn completes_tables_after_comma_in_from_clause() {
        let query = "SELECT * FROM users, ";

        let response = complete_sql(&catalog_request(query));

        assert!(
            response
                .items
                .iter()
                .any(|item| item.kind == CompletionKind::Table),
            "should suggest tables after comma in FROM clause"
        );
    }

    #[test]
    fn dialect_postgres_is_accepted() {
        let response = complete_sql(&request("SEL", 3));
        assert!(!response.items.is_empty());
    }

    #[test]
    fn dialect_sqlite_includes_sqlite_keywords() {
        let mut req = request("VAC", 3);
        req.dialect = "sqlite".to_string();

        let response = complete_sql(&req);

        assert!(response.items.iter().any(|item| item.label == "VACUUM"));
    }
}
