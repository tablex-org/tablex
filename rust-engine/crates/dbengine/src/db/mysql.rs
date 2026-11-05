use mysql_async::prelude::Queryable;
use mysql_async::{Opts, Pool};
use serde_json::json;

use super::mysql_arrow;

pub struct MySqlConn {
    pool: Pool,
}

impl MySqlConn {
    pub async fn connect(uri: &str) -> Result<Self, String> {
        validate_mysql_uri(uri)?;
        let normalized = normalize_mysql_uri(uri)?;
        let opts =
            Opts::from_url(&normalized).map_err(|e| format!("MySQL URI parse failed: {e}"))?;
        let pool = Pool::new(opts);
        let mut conn = pool
            .get_conn()
            .await
            .map_err(|e| format!("MySQL connect failed: {e}"))?;
        conn.ping()
            .await
            .map_err(|e| format!("MySQL ping failed: {e}"))?;
        drop(conn);
        Ok(Self { pool })
    }

    pub async fn disconnect(self) {
        let _ = self.pool.disconnect().await;
    }

    pub async fn list_databases(&self) -> Result<Vec<String>, String> {
        let mut conn = self
            .pool
            .get_conn()
            .await
            .map_err(|e| format!("MySQL get connection failed: {e}"))?;
        conn.query("SHOW DATABASES")
            .await
            .map_err(|e| format!("MySQL list databases failed: {e}"))
    }

    pub async fn list_tables(&self, db: &str) -> Result<Vec<String>, String> {
        let mut conn = self
            .pool
            .get_conn()
            .await
            .map_err(|e| format!("MySQL get connection failed: {e}"))?;
        conn.exec(
            "SELECT TABLE_NAME FROM information_schema.TABLES WHERE TABLE_SCHEMA = ? ORDER BY TABLE_NAME",
            (db,),
        )
        .await
        .map_err(|e| format!("MySQL list tables failed: {e}"))
    }

    pub async fn server_info(&self) -> Result<serde_json::Value, String> {
        let mut conn = self
            .pool
            .get_conn()
            .await
            .map_err(|e| format!("MySQL get connection failed: {e}"))?;
        let version: Option<String> = conn
            .query_first("SELECT VERSION()")
            .await
            .map_err(|e| format!("MySQL server info failed: {e}"))?;
        Ok(json!({ "version": version.unwrap_or_default() }))
    }

    pub async fn execute_sql(
        &self,
        db: &str,
        sql: &str,
        limit: i64,
    ) -> Result<arrow_array::RecordBatch, String> {
        let mut conn = self
            .pool
            .get_conn()
            .await
            .map_err(|e| format!("MySQL get connection failed: {e}"))?;
        if !db.trim().is_empty() {
            conn.query_drop(format!("USE `{}`", db.replace('`', "``")))
                .await
                .map_err(|e| format!("MySQL select database failed: {e}"))?;
        }
        let sql = mysql_sql_with_limit(sql.trim(), limit);
        let rows = conn
            .query(sql)
            .await
            .map_err(|e| format!("MySQL query failed: {e}"))?;
        mysql_arrow::rows_to_record_batch(rows)
    }
}

fn mysql_sql_with_limit(sql: &str, limit: i64) -> String {
    let limit = limit.max(1);
    let trimmed = sql.trim().trim_end_matches(';');
    if !starts_with_read_keyword(trimmed) || contains_limit_keyword(trimmed) {
        trimmed.to_string()
    } else {
        format!("{trimmed} LIMIT {limit}")
    }
}

fn starts_with_read_keyword(sql: &str) -> bool {
    matches!(
        sql.split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_uppercase()
            .as_str(),
        "SELECT" | "WITH" | "SHOW" | "DESCRIBE" | "DESC" | "EXPLAIN"
    )
}

fn contains_limit_keyword(sql: &str) -> bool {
    let mut in_word = false;
    let mut word_start = 0;
    for (idx, ch) in sql.char_indices() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            if !in_word {
                in_word = true;
                word_start = idx;
            }
        } else if in_word {
            if sql[word_start..idx].eq_ignore_ascii_case("limit") {
                return true;
            }
            in_word = false;
        }
    }
    in_word && sql[word_start..].eq_ignore_ascii_case("limit")
}

pub fn validate_mysql_uri(uri: &str) -> Result<(), String> {
    let lower = uri.trim().to_ascii_lowercase();
    if lower.starts_with("mysql://") || lower.starts_with("mariadb://") {
        Ok(())
    } else {
        Err("MySQL URI must start with mysql:// or mariadb://".to_string())
    }
}

pub fn normalize_mysql_uri(uri: &str) -> Result<String, String> {
    validate_mysql_uri(uri)?;
    let trimmed = uri.trim();
    if trimmed.to_ascii_lowercase().starts_with("mariadb://") {
        Ok(format!("mysql://{}", &trimmed["mariadb://".len()..]))
    } else {
        Ok(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::{mysql_sql_with_limit, normalize_mysql_uri, validate_mysql_uri};

    #[test]
    fn validate_mysql_uri_accepts_mysql_and_mariadb() {
        assert!(validate_mysql_uri("mysql://u:p@h:3306/db").is_ok());
        assert!(validate_mysql_uri("mariadb://u:p@h:3306/db").is_ok());
        assert!(validate_mysql_uri("postgres://x").is_err());
    }

    #[test]
    fn mysql_limit_detection_handles_newline_limit() {
        let sql = "SELECT *\nFROM users\nLIMIT 5";
        assert_eq!(mysql_sql_with_limit(sql, 200), sql);
    }

    #[test]
    fn normalize_mysql_uri_rewrites_case_insensitive_mariadb_scheme() {
        assert_eq!(
            normalize_mysql_uri("MariaDB://u:p@h:3306/db").unwrap(),
            "mysql://u:p@h:3306/db"
        );
    }
}
