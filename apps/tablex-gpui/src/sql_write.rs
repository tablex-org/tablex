//! Which SQL statements write. A read-only connection (see
//! `settings::connection`) refuses them in the SQL editor. The check is
//! deliberately conservative: only statements that start like a read (`SELECT`,
//! `WITH`, `SHOW`, ...) and contain no write keyword outside comments, string
//! literals and quoted names are let through; everything else counts as a
//! write.

use engine_driver_kit::sql::{tokens, SqlLexOptions, SqlTokenKind};

/// Statements that read when nothing in them writes.
const READ_LEADING: &[&str] = &[
    "SELECT", "WITH", "VALUES", "TABLE", "SHOW", "DESCRIBE", "DESC", "EXPLAIN", "USE",
];

/// Words that write wherever they appear in an otherwise read-looking
/// statement (`WITH x AS (...) DELETE ...`, `SELECT ... INTO t`,
/// `EXPLAIN ANALYZE UPDATE ...`, `SELECT ... FOR UPDATE`).
const WRITE_WORDS: &[&str] = &[
    "INSERT", "UPDATE", "DELETE", "MERGE", "INTO", "CREATE", "DROP", "ALTER", "TRUNCATE", "COPY",
    "CALL", "GRANT", "REVOKE", "REINDEX", "VACUUM", "LOCK",
];

/// The keyword that makes the first writing statement of `sql` write
/// (`"UPDATE"`, `"DROP"`, ...), or `None` when every statement only reads. `lex` is how
/// the connection's server reads the text (comments, quotes, `;`): a statement is what
/// the server sees, so a lexer for another dialect could hide a write behind a comment.
pub fn first_write_keyword(sql: &str, lex: &SqlLexOptions) -> Option<String> {
    tokens(sql, lex).tokens.split(|token| token.kind == SqlTokenKind::Semicolon).find_map(|statement| {
        let words: Vec<String> = statement
            .iter()
            .filter(|token| token.kind == SqlTokenKind::Word)
            .map(|token| sql[token.range.clone()].to_ascii_uppercase())
            .collect();
        let leading = words.first()?;
        if READ_LEADING.contains(&leading.as_str()) {
            words.iter().find(|word| WRITE_WORDS.contains(&word.as_str())).cloned()
        } else {
            Some(leading.clone())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocked(sql: &str) -> Option<String> {
        first_write_keyword(sql, &SqlLexOptions::postgres())
    }

    fn blocked_on(sql: &str, lex: &SqlLexOptions) -> Option<String> {
        first_write_keyword(sql, lex)
    }

    #[test]
    fn reads_pass() {
        for sql in [
            "SELECT * FROM items",
            "  select 1; select 2;",
            "-- note\nSELECT 1",
            "WITH c AS (SELECT 1) SELECT * FROM c",
            "SHOW TABLES",
            "DESCRIBE items",
            "EXPLAIN SELECT 1",
            "SELECT 'DELETE FROM x' AS note, \"update\" FROM t",
            "SELECT replace(name, 'a', 'b') FROM items",
            "SELECT 1 /* DROP TABLE x */",
            "(SELECT 1) UNION (SELECT 2)",
            "",
            "  ;; -- nothing",
        ] {
            assert_eq!(blocked(sql), None, "{sql}");
        }
    }

    #[test]
    fn writes_are_named_by_their_keyword() {
        assert_eq!(blocked("UPDATE items SET name = 'x'").as_deref(), Some("UPDATE"));
        assert_eq!(blocked("delete from items where id = 1").as_deref(), Some("DELETE"));
        assert_eq!(blocked("INSERT INTO items(name) VALUES ('a')").as_deref(), Some("INSERT"));
        assert_eq!(blocked("DROP TABLE items").as_deref(), Some("DROP"));
        assert_eq!(blocked("create table t(a int)").as_deref(), Some("CREATE"));
        assert_eq!(blocked("TRUNCATE items").as_deref(), Some("TRUNCATE"));
        assert_eq!(blocked("PRAGMA user_version = 3").as_deref(), Some("PRAGMA"));
    }

    #[test]
    fn a_write_hidden_behind_a_read_is_still_a_write() {
        assert_eq!(blocked("WITH d AS (DELETE FROM t RETURNING *) SELECT * FROM d").as_deref(), Some("DELETE"));
        assert_eq!(blocked("SELECT * INTO copy FROM items").as_deref(), Some("INTO"));
        assert_eq!(blocked("EXPLAIN ANALYZE UPDATE t SET a = 1").as_deref(), Some("UPDATE"));
        assert_eq!(blocked("SELECT * FROM t FOR UPDATE").as_deref(), Some("UPDATE"));
    }

    #[test]
    fn any_statement_of_a_script_counts() {
        assert_eq!(blocked("SELECT 1; DELETE FROM items; SELECT 2").as_deref(), Some("DELETE"));
        assert_eq!(blocked("SELECT 1; SELECT 'x;DROP'").as_deref(), None);
    }

    #[test]
    fn a_carriage_return_ends_a_line_comment_on_postgres_only() {
        // PostgreSQL ends `--x` at the `\r`, so the DELETE after it is a statement of its own.
        for sql in [
            "SELECT 2 -- x\r; DELETE FROM t WHERE id = 1",
            "-- c\rDELETE FROM t WHERE x = 1",
            "SELECT 1 --x\rSELECT 1; DROP TABLE t",
            "SELECT 1 -- x\r, pg_sleep(1); UPDATE t SET a = 1",
        ] {
            assert!(blocked(sql).is_some(), "{sql:?}");
        }
        assert_eq!(blocked("SELECT 2 -- x\r; DELETE FROM t WHERE id = 1").as_deref(), Some("DELETE"));
        assert_eq!(blocked("-- c\rDELETE FROM t WHERE x = 1").as_deref(), Some("DELETE"));
        // The other engines run the comment to the line feed: it is all one comment there.
        for lex in [SqlLexOptions::sqlite(), SqlLexOptions::mysql(), SqlLexOptions::clickhouse()] {
            assert_eq!(blocked_on("SELECT 2 -- x\r; DELETE FROM t WHERE id = 1", &lex), None);
            assert_eq!(blocked_on("-- c\rDELETE FROM t WHERE x = 1", &lex), None);
        }
    }

    #[test]
    fn the_servers_own_quoting_decides_what_is_a_word() {
        // MySQL: backslash escapes, `#` comments, executable comments are code.
        let mysql = SqlLexOptions::mysql();
        assert_eq!(blocked_on(r"SELECT 'a\' ; DELETE FROM t -- '", &mysql), None);
        assert_eq!(blocked_on("SELECT 1 # x\n; DELETE FROM t", &mysql).as_deref(), Some("DELETE"));
        assert_eq!(blocked_on("SELECT 1 /*!50000 ; DELETE FROM t */", &mysql).as_deref(), Some("DELETE"));
        assert_eq!(blocked("SELECT $q$ DELETE ; $q$"), None);
    }
}
