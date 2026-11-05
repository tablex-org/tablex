//! What smart commit reads from a statement: whether it modifies, so a tab on its own session in
//! auto-commit starts a transaction first (`SET autocommit = 0` on MySQL / MariaDB, a `BEGIN` before
//! it on PostgreSQL / SQLite: see `app::transaction`).
//!
//! The text is read lazily, one token at a time with nothing kept, and decided from the first words
//! of each statement: an `INSERT` of megabytes is judged by its first token. Only a statement that
//! starts like a read (`WITH`, `SELECT`, ...) is read to its end, looking for a word that writes, and
//! none of it allocates per word.
//!
//! A statement is left alone when it changes no data (`SET`, `PRAGMA`, `VACUUM`), when the
//! server refuses it inside a transaction (PostgreSQL's `CREATE INDEX CONCURRENTLY`, `REINDEX
//! DATABASE`, `ALTER SYSTEM`, `CREATE DATABASE`...; SQLite's `ATTACH`), or when MySQL / MariaDB
//! commit before and after it (DDL, `LOCK TABLES`, account statements: the change would commit
//! whatever the tab does, and "rolled back" would be false assurance).

use super::file_run::head::{commits_implicitly, Comments};
use crate::connections::ConnectionKind;
use engine_driver_kit::sql::{token_iter, SqlLexOptions, SqlToken, SqlTokenKind};

/// What smart commit makes of a text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Smart {
    /// Nothing in it modifies.
    No,
    /// Something in it modifies: the tab switches to manual first.
    Modifies,
    /// It sets `autocommit`, which a manual tab refuses: the tab is left as it is.
    Veto,
}

/// Statements that read when nothing in them writes.
const READ_LEADING: [&str; 9] = ["SELECT", "WITH", "VALUES", "TABLE", "SHOW", "DESCRIBE", "DESC", "EXPLAIN", "USE"];

/// Words that write wherever they appear in a read-looking statement (`WITH x AS (...) DELETE ...`,
/// `SELECT ... INTO t`, `EXPLAIN ANALYZE UPDATE ...`, `SELECT ... FOR UPDATE`).
const WRITE_WORDS: [&str; 16] = [
    "INSERT", "UPDATE", "DELETE", "MERGE", "INTO", "CREATE", "DROP", "ALTER", "TRUNCATE", "COPY", "CALL", "GRANT", "REVOKE",
    "REINDEX", "VACUUM", "LOCK",
];

/// Statements that begin, end or divide the user's own transaction.
const TRANSACTION_LEADING: [&str; 8] = ["BEGIN", "START", "COMMIT", "END", "ROLLBACK", "ABORT", "SAVEPOINT", "RELEASE"];

/// Statements that modify whatever follows them.
const IMMEDIATE: [&str; 7] = ["INSERT", "UPDATE", "DELETE", "MERGE", "REPLACE", "CALL", "COPY"];

fn is(word: &str, expected: &str) -> bool {
    word.eq_ignore_ascii_case(expected)
}

fn any(word: &str, list: &[&str]) -> bool {
    list.iter().any(|expected| is(word, expected))
}

/// How MySQL and MariaDB read comments, as the lexer the session reads with (`lex`) does: the server's
/// version decides which versioned comments are code, `ANSI_QUOTES` which quote names. `None` for the
/// other kinds.
fn comments_of(kind: ConnectionKind, lex: &SqlLexOptions) -> Option<Comments> {
    matches!(kind, ConnectionKind::Mysql | ConnectionKind::Mariadb).then_some(Comments {
        version: lex.server_version,
        mariadb: kind == ConnectionKind::Mariadb,
        ansi_quotes: lex.double_quote_is_identifier,
    })
}

/// Whether MySQL / MariaDB commit by themselves around the statement of `sql` that starts at byte
/// `start` (its first token, comments and blanks before it skipped by the lexer).
fn commits_at(sql: &str, start: usize, lex: &SqlLexOptions, kind: ConnectionKind) -> bool {
    let text = sql.as_bytes();
    comments_of(kind, lex).is_some_and(|comments| commits_implicitly(&text[start..(start + 256).min(text.len())], comments))
}

/// [`commits_at`] for the first statement of `sql`: what the transaction log says about a statement
/// sent as typed. Read from its first token, so a long comment before it does not hide it, and a
/// versioned comment the server skips does not stand for it.
pub(super) fn commits_by_itself(sql: &str, lex: &SqlLexOptions, kind: ConnectionKind) -> bool {
    comments_of(kind, lex).is_some()
        && token_iter(sql, lex).next().is_some_and(|first| commits_at(sql, first.range.start, lex, kind))
}

/// The first words of a statement and what was seen further in, gathered token by token.
#[derive(Default)]
struct Statement<'a> {
    words: [&'a str; 6],
    count: usize,
    started: bool,
    start: usize,
    concurrently: bool,
    autocommit: bool,
    write_word: bool,
    lead_reads: bool,
}

impl<'a> Statement<'a> {
    fn push(&mut self, sql: &'a str, token: &SqlToken) {
        if !self.started {
            self.started = true;
            self.start = token.range.start;
        }
        if token.kind != SqlTokenKind::Word {
            return;
        }
        let word = &sql[token.range.clone()];
        if self.count < self.words.len() {
            self.words[self.count] = word;
            if self.count == 0 {
                self.lead_reads = any(word, &READ_LEADING);
            }
            self.count += 1;
        }
        if self.lead_reads && any(word, &WRITE_WORDS) {
            self.write_word = true;
        }
        self.concurrently |= is(word, "CONCURRENTLY");
        self.autocommit |= is(word, "AUTOCOMMIT");
    }

    fn lead(&self) -> Option<&'a str> {
        (self.count > 0).then_some(self.words[0])
    }

    fn word(&self, index: usize) -> Option<&'a str> {
        (index < self.count).then_some(self.words[index])
    }

    /// Settled by its first word: a write whatever follows.
    fn modifies_at_once(&self) -> bool {
        self.count >= 1 && any(self.words[0], &IMMEDIATE)
    }

    fn verdict(&self, sql: &str, lex: &SqlLexOptions, kind: ConnectionKind) -> Smart {
        let Some(lead) = self.lead() else { return Smart::No };
        if is(lead, "SET") && self.autocommit {
            return Smart::Veto;
        }
        if any(lead, &TRANSACTION_LEADING) || (is(lead, "PREPARE") && self.word(1).is_some_and(|word| is(word, "TRANSACTION"))) {
            return Smart::No;
        }
        // No data changes, or the server refuses it inside a transaction.
        if any(lead, &["SET", "PRAGMA", "VACUUM"]) {
            return Smart::No;
        }
        if self.lead_reads {
            return if self.write_word { Smart::Modifies } else { Smart::No };
        }
        let second = self.word(1);
        if any(lead, &["CREATE", "DROP"]) && second.is_some_and(|word| any(word, &["DATABASE", "TABLESPACE"])) {
            return Smart::No;
        }
        match kind {
            ConnectionKind::Postgres => {
                if self.concurrently && any(lead, &["CREATE", "DROP", "REINDEX"]) {
                    return Smart::No;
                }
                if is(lead, "REINDEX") && (1..self.count).any(|index| any(self.words[index], &["DATABASE", "SYSTEM"])) {
                    return Smart::No;
                }
                if (is(lead, "ALTER") && second.is_some_and(|word| is(word, "SYSTEM"))) || is(lead, "DISCARD") {
                    return Smart::No;
                }
                // `CLUSTER` of the whole database (no table) cannot run in a transaction.
                if is(lead, "CLUSTER") && second.is_none_or(|word| is(word, "VERBOSE")) {
                    return Smart::No;
                }
            }
            ConnectionKind::Sqlite if any(lead, &["ATTACH", "DETACH"]) => return Smart::No,
            ConnectionKind::Mysql | ConnectionKind::Mariadb if commits_at(sql, self.start, lex, kind) => return Smart::No,
            _ => {}
        }
        Smart::Modifies
    }
}

/// Texts up to this size are read to their end even when their first word settles that they
/// modify, so a `SET autocommit` behind it is seen (`UPDATE t ...; SET autocommit = 1`).
const VETO_SCAN_BYTES: usize = 64 * 1024;

/// Whether smart commit takes `sql` for a modification (see the module's note).
pub(super) fn smart_verdict(sql: &str, lex: &SqlLexOptions, kind: ConnectionKind) -> Smart {
    let mut statement = Statement::default();
    let mut modifies = false;
    for token in token_iter(sql, lex) {
        if token.kind == SqlTokenKind::Semicolon {
            match statement.verdict(sql, lex, kind) {
                Smart::Veto => return Smart::Veto,
                Smart::Modifies => modifies = true,
                Smart::No => {}
            }
            statement = Statement::default();
            continue;
        }
        statement.push(sql, &token);
        // Settled by its first word, whatever follows (and none of these commits by itself).
        if statement.modifies_at_once() {
            if sql.len() > VETO_SCAN_BYTES {
                return Smart::Modifies;
            }
            modifies = true;
        }
    }
    match statement.verdict(sql, lex, kind) {
        Smart::Veto => Smart::Veto,
        Smart::Modifies => Smart::Modifies,
        Smart::No if modifies => Smart::Modifies,
        Smart::No => Smart::No,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ConnectionKind::{Mariadb, Mysql, Postgres, Sqlite};

    fn verdict(sql: &str, kind: ConnectionKind) -> Smart {
        let lex = match kind {
            Mysql | Mariadb => SqlLexOptions::mysql(),
            Sqlite => SqlLexOptions::sqlite(),
            _ => SqlLexOptions::postgres(),
        };
        smart_verdict(sql, &lex, kind)
    }

    #[test]
    fn smart_commit_takes_writes_for_modifications_and_nothing_else() {
        for sql in [
            "UPDATE t SET a = 1",
            "insert into t values (1)",
            "DELETE FROM t",
            "CREATE TABLE t (a int)",
            "DROP TABLE t",
            "TRUNCATE t",
            "WITH x AS (SELECT 1) DELETE FROM t",
            "SET x = 1; UPDATE t SET a = 1",
            "-- note\n UPDATE t SET a = 1",
            "BEGIN; UPDATE a SET x = 1; COMMIT; UPDATE b SET y = 2",
            "SELECT * FROM t FOR UPDATE",
        ] {
            assert_eq!(verdict(sql, Postgres), Smart::Modifies, "{sql}");
        }
        for sql in [
            "SELECT 1",
            "WITH x AS (SELECT 1) SELECT * FROM x",
            "SHOW TABLES",
            "EXPLAIN SELECT 1",
            "SET search_path = a",
            "PRAGMA foreign_keys = ON",
            "VACUUM",
            "BEGIN",
            "COMMIT",
            "ROLLBACK",
            "SAVEPOINT a",
            "SELECT 'UPDATE t'",
            "SELECT update_count FROM t",
            "",
            "   -- nothing\n",
        ] {
            assert_eq!(verdict(sql, Postgres), Smart::No, "{sql}");
        }
        assert_eq!(verdict("SET autocommit = 0; UPDATE t SET a = 1", Mysql), Smart::Veto);
        assert_eq!(verdict("UPDATE t SET a = 1; SET autocommit = 1", Mysql), Smart::Veto, "a veto behind a settled statement");
        assert_eq!(verdict("set @@session.autocommit = 1", Mysql), Smart::Veto);
    }

    #[test]
    fn what_a_server_refuses_inside_a_transaction_is_left_alone() {
        for sql in [
            "CREATE INDEX CONCURRENTLY i ON t (a)",
            "CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS i ON t (a)",
            "DROP INDEX CONCURRENTLY i",
            "REINDEX INDEX CONCURRENTLY i",
            "REINDEX DATABASE d",
            "REINDEX (VERBOSE) SYSTEM d",
            "ALTER SYSTEM SET work_mem = '1GB'",
            "DISCARD ALL",
            "CLUSTER",
            "CLUSTER VERBOSE",
            "CREATE DATABASE d",
            "DROP DATABASE d",
            "CREATE TABLESPACE s LOCATION '/x'",
            "VACUUM t",
        ] {
            assert_eq!(verdict(sql, Postgres), Smart::No, "{sql}");
        }
        for sql in ["CREATE INDEX i ON t (a)", "REINDEX TABLE t", "CLUSTER t USING i", "ALTER TABLE t ADD c int"] {
            assert_eq!(verdict(sql, Postgres), Smart::Modifies, "{sql}");
        }
        assert_eq!(verdict("ATTACH DATABASE 'x.db' AS x", Sqlite), Smart::No);
        assert_eq!(verdict("DETACH x", Sqlite), Smart::No);
        assert_eq!(verdict("CREATE INDEX i ON t (a)", Sqlite), Smart::Modifies);
    }

    #[test]
    fn mysql_statements_that_commit_by_themselves_do_not_switch() {
        for kind in [Mysql, Mariadb] {
            for sql in [
                "CREATE TABLE t (a int)",
                "ALTER TABLE t ADD c int",
                "DROP TABLE t",
                "RENAME TABLE a TO b",
                "TRUNCATE TABLE t",
                "LOCK TABLES t WRITE",
                "GRANT SELECT ON t TO u",
                "/*!40000 ALTER TABLE t DISABLE KEYS */",
            ] {
                assert_eq!(verdict(sql, kind), Smart::No, "{kind:?} {sql}");
            }
            for sql in ["INSERT INTO t VALUES (1)", "UPDATE t SET a = 1", "DELETE FROM t", "REPLACE INTO t VALUES (1)", "CREATE TEMPORARY TABLE t (a int)"] {
                assert_eq!(verdict(sql, kind), Smart::Modifies, "{kind:?} {sql}");
            }
        }
    }

    /// What the server commits by itself is read from where the lexer says the statement starts, with the
    /// server's version: the app used to scan the first 256 bytes of the text with a version-less scanner.
    #[test]
    fn what_commits_by_itself_is_read_with_the_options_of_the_session() {
        let mysql = SqlLexOptions::mysql().with_server_version(80411);
        let mariadb = SqlLexOptions::mariadb().with_server_version(110809);
        // A versioned comment above the server's version is a comment: the INSERT after it commits nothing.
        assert!(!commits_by_itself("/*!99999 DROP TABLE x */ INSERT INTO t VALUES (1)", &mysql, Mysql));
        assert!(!commits_by_itself("/*M!999999 ALTER TABLE t ADD c int */ SELECT 1", &mariadb, Mariadb));
        // ... and at or below it, code.
        assert!(commits_by_itself("/*!40000 ALTER TABLE t DISABLE KEYS */", &mysql, Mysql));
        assert!(commits_by_itself("/*M!100000 ALTER TABLE t ADD c int */", &mariadb, Mariadb));
        // A long comment before the statement does not hide it; `#` and `--` comments do not either.
        let long = format!("/* {} */ ALTER TABLE t ADD c int", "x".repeat(400));
        assert!(commits_by_itself(&long, &mysql, Mysql));
        assert!(commits_by_itself("# note\n  CREATE TABLE t (a int)", &mysql, Mysql));
        assert!(commits_by_itself("-- note\nDROP TABLE t", &mysql, Mysql));
        assert!(!commits_by_itself("# CREATE TABLE t (a int)\nINSERT INTO t VALUES (1)", &mysql, Mysql));
        assert!(!commits_by_itself("CREATE TEMPORARY TABLE t (a int)", &mysql, Mysql));
        assert!(!commits_by_itself("", &mysql, Mysql));
        // The other kinds never commit by themselves.
        assert!(!commits_by_itself("CREATE TABLE t (a int)", &SqlLexOptions::postgres(), Postgres));
        assert!(!commits_by_itself("CREATE TABLE t (a int)", &SqlLexOptions::sqlite(), Sqlite));
    }

    /// The verdict reads the text with the options the session reads it with: the same text is one thing under
    /// one set of options and another under the next (the lexer is the driver's own: `engine-driver-kit`).
    #[test]
    fn the_verdict_follows_the_lexer_options_of_the_session() {
        let mysql = SqlLexOptions::mysql();
        let postgres = SqlLexOptions::postgres();
        // NO_BACKSLASH_ESCAPES: `'a\'` is a whole string and the UPDATE after it runs; otherwise the string never ends.
        let escaped = "SELECT 'a\\'; UPDATE t SET x = 1";
        assert_eq!(smart_verdict(escaped, &mysql, Mysql), Smart::No);
        assert_eq!(smart_verdict(escaped, &mysql.without_backslash_escapes(), Mysql), Smart::Modifies);
        // ANSI_QUOTES: a backslash escapes nothing in `"a\"`, which ends there.
        let quoted = "SELECT \"a\\\"; UPDATE t SET x = 1; SELECT \"b\"";
        assert_eq!(smart_verdict(quoted, &mysql, Mysql), Smart::No);
        assert_eq!(smart_verdict(quoted, &mysql.with_ansi_quotes(), Mysql), Smart::Modifies);
        // Dollar quotes are PostgreSQL's: the UPDATE inside one is text there, a statement elsewhere.
        for dollar in ["SELECT $$ UPDATE t $$", "SELECT $a$ ; UPDATE t SET x = 1 $a$"] {
            assert_eq!(smart_verdict(dollar, &postgres, Postgres), Smart::No, "{dollar}");
        }
        assert_eq!(smart_verdict("SELECT $a$ ; UPDATE t SET x = 1 $a$", &mysql, Mysql), Smart::Modifies);
        // Block comments nest in PostgreSQL only.
        let nested = "/* /* */ UPDATE t SET a = 1 */";
        assert_eq!(smart_verdict(nested, &postgres, Postgres), Smart::No);
        assert_eq!(smart_verdict(nested, &mysql, Mysql), Smart::Modifies);
        // `#` starts a comment in MySQL and MariaDB.
        let hash = "# UPDATE t SET a = 1\nSELECT 1";
        assert_eq!(smart_verdict(hash, &mysql, Mysql), Smart::No);
        assert_eq!(smart_verdict(hash, &postgres, Postgres), Smart::Modifies);
    }

    #[test]
    fn a_huge_text_is_judged_from_its_first_words_and_a_huge_read_in_one_cheap_pass() {
        let rows = "(1, 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'),".repeat(200_000);
        let insert = format!("INSERT INTO t VALUES {rows}(2, 'z')");
        assert!(insert.len() > 7_000_000);
        let started = std::time::Instant::now();
        assert_eq!(verdict(&insert, Postgres), Smart::Modifies);
        assert!(started.elapsed() < std::time::Duration::from_millis(20), "{:?}", started.elapsed());
        // A read is read to its end (a write word could be anywhere): one pass, no allocation per word.
        let select = format!("SELECT {} 1", "a, ".repeat(1_500_000));
        let started = std::time::Instant::now();
        assert_eq!(verdict(&select, Postgres), Smart::No);
        let one = started.elapsed();
        let select = format!("SELECT {} 1", "a, ".repeat(3_000_000));
        let started = std::time::Instant::now();
        assert_eq!(verdict(&select, Postgres), Smart::No);
        let two = started.elapsed();
        assert!(one < std::time::Duration::from_secs(3), "{one:?}");
        assert!(two < one * 4, "twice the text took {two:?} against {one:?}: not linear");
    }
}
