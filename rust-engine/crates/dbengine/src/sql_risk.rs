use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SqlRiskCategory {
    read = 0,
    low_risk_write = 1,
    write = 2,
    schema_change = 3,
    unknown = 4,
    dangerous = 5,
}

#[derive(Debug, Clone, Serialize)]
pub struct SqlStatementInfo {
    pub category: SqlRiskCategory,
    pub keyword: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SqlRiskReport {
    pub category: String,
    pub statement_categories: Vec<String>,
    pub multi_statement: bool,
}

fn category_rank(c: SqlRiskCategory) -> u8 {
    match c {
        SqlRiskCategory::read => 0,
        SqlRiskCategory::low_risk_write => 1,
        SqlRiskCategory::write => 2,
        SqlRiskCategory::schema_change => 3,
        SqlRiskCategory::unknown => 4,
        SqlRiskCategory::dangerous => 5,
    }
}

fn max_category(a: SqlRiskCategory, b: SqlRiskCategory) -> SqlRiskCategory {
    if category_rank(b) > category_rank(a) {
        b
    } else {
        a
    }
}

fn split_statements(sql: &str) -> Vec<String> {
    let mut statements = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = sql.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        let c = chars[i];

        if c == '\'' {
            current.push(c);
            i += 1;
            while i < len {
                let cc = chars[i];
                current.push(cc);
                i += 1;
                if cc == '\'' {
                    if i < len && chars[i] == '\'' {
                        current.push(chars[i]);
                        i += 1;
                    } else {
                        break;
                    }
                }
            }
            continue;
        }

        if c == '-' && i + 1 < len && chars[i + 1] == '-' {
            while i < len && chars[i] != '\n' {
                current.push(chars[i]);
                i += 1;
            }
            continue;
        }

        if c == '/' && i + 1 < len && chars[i + 1] == '*' {
            current.push(chars[i]);
            current.push(chars[i + 1]);
            i += 2;
            while i + 1 < len {
                if chars[i] == '*' && chars[i + 1] == '/' {
                    current.push(chars[i]);
                    current.push(chars[i + 1]);
                    i += 2;
                    break;
                }
                current.push(chars[i]);
                i += 1;
            }
            continue;
        }

        if c == '$' {
            let tag_start = i;
            let mut tag_end = i;
            if i + 1 < len && chars[i + 1] == '$' {
                tag_end = i + 1;
            } else {
                let mut j = i + 1;
                while j < len && (chars[j].is_ascii_alphanumeric() || chars[j] == '_') {
                    j += 1;
                }
                if j < len && j > i + 1 && chars[j] == '$' && j + 1 < len && chars[j + 1] == '$' {
                    tag_end = j + 1;
                }
            }
            if tag_end > tag_start {
                let tag: String = chars[tag_start..=tag_end].iter().collect();
                for ch in tag.chars() {
                    current.push(ch);
                }
                i = tag_end + 1;
                let close_tag = if tag == "$$" { "$$" } else { &tag };
                while i < len {
                    if chars[i] == '$' {
                        let mut match_len = 0;
                        let close_chars: Vec<char> = close_tag.chars().collect();
                        for (k, &mc) in close_chars.iter().enumerate() {
                            if i + k < len && chars[i + k] == mc {
                                match_len = k + 1;
                            } else {
                                match_len = 0;
                                break;
                            }
                        }
                        if match_len == close_chars.len() {
                            for &ch in close_chars.iter() {
                                current.push(ch);
                            }
                            i += close_chars.len();
                            break;
                        } else {
                            current.push(chars[i]);
                            i += 1;
                        }
                    } else {
                        current.push(chars[i]);
                        i += 1;
                    }
                }
                continue;
            }
        }

        if c == ';' {
            let trimmed = current.trim().to_string();
            if !trimmed.is_empty() {
                statements.push(trimmed);
            }
            current.clear();
            i += 1;
            continue;
        }

        current.push(c);
        i += 1;
    }

    let trimmed = current.trim().to_string();
    if !trimmed.is_empty() {
        statements.push(trimmed);
    }

    statements
}

fn extract_leading_keyword(stmt: &str) -> String {
    let s = stmt.trim_start();
    let keyword: String = s
        .chars()
        .take_while(|c| c.is_ascii_alphabetic() || *c == '_')
        .collect();
    keyword.to_uppercase()
}

fn is_scoped_where(stmt: &str) -> bool {
    let upper = stmt.to_uppercase();
    let where_pos = match upper.find("WHERE") {
        Some(p) => p,
        None => return false,
    };

    let after_where = &upper[where_pos + 5..].trim_start();

    if after_where.starts_with("1=1") || after_where.starts_with("1 = 1") {
        return false;
    }
    if after_where.starts_with("TRUE")
        || after_where.starts_with("(TRUE")
        || after_where.starts_with("(TRUE)")
        || after_where.starts_with("( TRUE")
    {
        return false;
    }

    let key_col_patterns = &[
        "ID =", "ID =", "_ID =", "ID=", "UUID =", "UUID=", "KEY =", "KEY=", "_ID=", "UUID=",
        ".ID=", ".ID =",
    ];
    for pat in key_col_patterns {
        if after_where.contains(pat) {
            return true;
        }
    }

    let key_col_prefixes = &[
        "ID =", "ID=", "_ID =", "_ID=", "UUID =", "UUID=", "KEY =", "KEY=",
    ];
    for pat in key_col_prefixes {
        if after_where.contains(pat) {
            return true;
        }
    }

    false
}

fn classify_do_block(stmt: &str) -> SqlRiskCategory {
    let inner = extract_do_block_body(stmt);
    let inner = match inner {
        Some(s) => s,
        None => return SqlRiskCategory::dangerous,
    };

    let upper = inner.to_uppercase();

    if contains_dangerous_keyword(&upper) {
        return SqlRiskCategory::dangerous;
    }

    if upper.contains("CREATE") {
        return SqlRiskCategory::schema_change;
    }

    let mut result = SqlRiskCategory::low_risk_write;

    if contains_unscoped_update(&inner) {
        return SqlRiskCategory::dangerous;
    }

    if upper.contains("DELETE") || upper.contains("MERGE") || upper.contains("REPLACE") {
        result = SqlRiskCategory::write;
    }

    if upper.contains("INSERT") || upper.contains("UPDATE") {
        result = max_category(result, SqlRiskCategory::low_risk_write);
    }

    if result == SqlRiskCategory::low_risk_write
        && !upper.contains("INSERT")
        && !upper.contains("UPDATE")
        && !upper.contains("DELETE")
        && !upper.contains("MERGE")
        && !upper.contains("REPLACE")
    {
        result = SqlRiskCategory::unknown;
    }

    result
}

fn contains_dangerous_keyword(upper: &str) -> bool {
    for kw in &["DROP", "TRUNCATE", "ALTER", "RENAME"] {
        if upper.contains(kw) {
            return true;
        }
    }
    false
}

fn contains_unscoped_update(inner: &str) -> bool {
    let upper = inner.to_uppercase();
    let mut pos = 0;
    while let Some(idx) = upper[pos..].find("UPDATE") {
        let abs_idx = pos + idx;
        if abs_idx > 0 {
            let prev = upper.as_bytes()[abs_idx - 1];
            if prev.is_ascii_alphabetic() || prev == b'_' {
                pos = abs_idx + 6;
                continue;
            }
        }
        if abs_idx + 6 < upper.len() {
            let next = upper.as_bytes()[abs_idx + 6];
            if next.is_ascii_alphabetic() || next == b'_' {
                pos = abs_idx + 6;
                continue;
            }
        }
        let after = &upper[abs_idx + 6..];
        if !is_scoped_where_from(after) {
            return true;
        }
        pos = abs_idx + 6;
    }
    false
}

fn is_scoped_where_from(after_update_upper: &str) -> bool {
    let where_pos = match after_update_upper.find("WHERE") {
        Some(p) => p,
        None => return false,
    };

    let after_where = after_update_upper[where_pos + 5..].trim_start();

    if after_where.starts_with("1=1") || after_where.starts_with("1 = 1") {
        return false;
    }
    if after_where.starts_with("TRUE") {
        return false;
    }

    let key_patterns = &[
        "ID =", "ID=", "_ID =", "_ID=", "UUID =", "UUID=", "KEY =", "KEY=",
    ];
    for pat in key_patterns {
        if after_where.contains(pat) {
            return true;
        }
    }

    false
}

fn extract_do_block_body(stmt: &str) -> Option<String> {
    let upper = stmt.to_uppercase();
    let dollar_pos = upper.find("$$")?;
    let after_dollar = &stmt[dollar_pos + 2..];

    let end_pos = after_dollar.find("$$")?;
    Some(after_dollar[..end_pos].to_string())
}

fn classify_statement(stmt: &str) -> SqlStatementInfo {
    let upper_stmt = stmt.to_uppercase();
    let trimmed = upper_stmt.trim_start();

    if trimmed.starts_with("DO") {
        return SqlStatementInfo {
            category: classify_do_block(stmt),
            keyword: "DO".to_string(),
        };
    }

    let keyword = extract_leading_keyword(stmt);
    let category = match keyword.as_str() {
        "SELECT" | "WITH" | "SHOW" | "DESCRIBE" | "DESC" | "EXPLAIN" => SqlRiskCategory::read,
        "DROP" | "TRUNCATE" | "ALTER" | "RENAME" => SqlRiskCategory::dangerous,
        "CREATE" => SqlRiskCategory::schema_change,
        "INSERT" => SqlRiskCategory::low_risk_write,
        "UPDATE" => {
            if is_scoped_where(stmt) {
                SqlRiskCategory::low_risk_write
            } else {
                SqlRiskCategory::dangerous
            }
        }
        "DELETE" | "MERGE" | "REPLACE" => SqlRiskCategory::write,
        _ => SqlRiskCategory::unknown,
    };

    SqlStatementInfo { category, keyword }
}

pub fn classify_sql_risk(sql: &str) -> SqlRiskReport {
    let statements = split_statements(sql);

    if statements.is_empty() {
        return SqlRiskReport {
            category: "unknown".to_string(),
            statement_categories: vec![],
            multi_statement: false,
        };
    }

    let mut infos: Vec<SqlStatementInfo> = Vec::new();
    let mut aggregate = SqlRiskCategory::read;

    for stmt in &statements {
        let info = classify_statement(stmt);
        aggregate = max_category(aggregate, info.category);
        infos.push(info);
    }

    SqlRiskReport {
        category: serde_json::to_value(aggregate)
            .ok()
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_else(|| "unknown".to_string()),
        statement_categories: infos
            .iter()
            .map(|i| {
                serde_json::to_value(i.category)
                    .ok()
                    .and_then(|v| v.as_str().map(|s| s.to_string()))
                    .unwrap_or_else(|| "unknown".to_string())
            })
            .collect(),
        multi_statement: statements.len() >= 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_select_read() {
        let report = classify_sql_risk("SELECT count(*) FROM users");
        assert_eq!(report.category, "read");
    }

    #[test]
    fn test_drop_dangerous() {
        let report = classify_sql_risk("DROP TABLE users");
        assert_eq!(report.category, "dangerous");
    }

    #[test]
    fn test_insert_low_risk_write() {
        let report = classify_sql_risk("INSERT INTO users(name) VALUES ('a')");
        assert_eq!(report.category, "low_risk_write");
    }

    #[test]
    fn test_update_scoped_where() {
        let report = classify_sql_risk("UPDATE users SET name='a' WHERE id=1");
        assert_eq!(report.category, "low_risk_write");
    }

    #[test]
    fn test_update_unscoped_dangerous() {
        let report = classify_sql_risk("UPDATE users SET name='a'");
        assert_eq!(report.category, "dangerous");
    }

    #[test]
    fn test_update_where_1_equals_1_dangerous() {
        let report = classify_sql_risk("UPDATE users SET name='a' WHERE 1=1");
        assert_eq!(report.category, "dangerous");
    }

    #[test]
    fn test_dollar_quote_no_split() {
        let report = classify_sql_risk("DO $$ BEGIN UPDATE t SET x=1; END $$;");
        assert!(!report.multi_statement);
    }

    #[test]
    fn test_string_literal_semicolon() {
        let report = classify_sql_risk("INSERT INTO t(s) VALUES ('a;b')");
        assert!(!report.multi_statement);
        assert_eq!(report.category, "low_risk_write");
    }

    #[test]
    fn test_multi_statement_dangerous() {
        let report = classify_sql_risk("SELECT 1; DROP TABLE users;");
        assert_eq!(report.category, "dangerous");
        assert!(report.multi_statement);
    }

    #[test]
    fn test_create_schema_change() {
        let report = classify_sql_risk("CREATE TABLE foo (id int)");
        assert_eq!(report.category, "schema_change");
    }

    #[test]
    fn test_delete_write() {
        let report = classify_sql_risk("DELETE FROM users WHERE id = 1");
        assert_eq!(report.category, "write");
    }

    #[test]
    fn test_alter_dangerous() {
        let report = classify_sql_risk("ALTER TABLE users ADD COLUMN email text");
        assert_eq!(report.category, "dangerous");
    }

    #[test]
    fn test_with_read() {
        let report = classify_sql_risk("WITH cte AS (SELECT 1) SELECT * FROM cte");
        assert_eq!(report.category, "read");
    }

    #[test]
    fn test_show_read() {
        let report = classify_sql_risk("SHOW TABLES");
        assert_eq!(report.category, "read");
    }

    #[test]
    fn test_explain_read() {
        let report = classify_sql_risk("EXPLAIN SELECT * FROM users");
        assert_eq!(report.category, "read");
    }

    #[test]
    fn test_truncate_dangerous() {
        let report = classify_sql_risk("TRUNCATE TABLE users");
        assert_eq!(report.category, "dangerous");
    }

    #[test]
    fn test_rename_dangerous() {
        let report = classify_sql_risk("RENAME TABLE old TO new");
        assert_eq!(report.category, "dangerous");
    }

    #[test]
    fn test_merge_write() {
        let report = classify_sql_risk("MERGE INTO target USING source ON target.id = source.id WHEN MATCHED THEN UPDATE SET target.x = source.x");
        assert_eq!(report.category, "write");
    }

    #[test]
    fn test_block_comment_no_split() {
        let report = classify_sql_risk("SELECT /* ; */ 1");
        assert!(!report.multi_statement);
        assert_eq!(report.category, "read");
    }

    #[test]
    fn test_line_comment_no_split() {
        let report = classify_sql_risk("SELECT 1 -- ; drop table\nFROM users");
        assert!(!report.multi_statement);
        assert_eq!(report.category, "read");
    }

    #[test]
    fn test_empty_input() {
        let report = classify_sql_risk("");
        assert_eq!(report.category, "unknown");
        assert!(!report.multi_statement);
    }

    #[test]
    fn test_unknown_keyword() {
        let report = classify_sql_risk("VACUUM ANALYZE users");
        assert_eq!(report.category, "unknown");
    }

    #[test]
    fn test_update_uuid_scoped() {
        let report = classify_sql_risk("UPDATE users SET name='a' WHERE uuid=1");
        assert_eq!(report.category, "low_risk_write");
    }

    #[test]
    fn test_update_key_scoped() {
        let report = classify_sql_risk("UPDATE users SET val='x' WHERE key=1");
        assert_eq!(report.category, "low_risk_write");
    }

    #[test]
    fn test_do_block_dangerous_unscoped() {
        let report = classify_sql_risk("DO $$ BEGIN UPDATE t SET x=1; END $$;");
        assert_eq!(report.category, "dangerous");
    }

    #[test]
    fn test_multi_statement_categories() {
        let report = classify_sql_risk("SELECT 1; INSERT INTO t(x) VALUES (1);");
        assert_eq!(report.statement_categories, vec!["read", "low_risk_write"]);
        assert!(report.multi_statement);
    }
}
