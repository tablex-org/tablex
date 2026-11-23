//! Statement classification for the agent policy.
//!
//! SQL goes through the same lexer the drivers use (`engine_driver_kit::sql`):
//! a statement is a **read** only when the lexer proves it is a single pure
//! read; everything else, including what it cannot recognise, is a **write**.
//!
//! Redis mirrors the driver's `classify_command` (the driver crate is a cdylib
//! with heavy dependencies, so the verb lists are copied here and a test pins
//! them to the driver's function). The mirror is never more permissive than
//! the driver: `PUBLISH` and `AUTH`, which the driver lets through as reads,
//! are writes for an agent, and server-side code (`FUNCTION`, `FCALL`, `DEBUG`,
//! ...) is blocked for it.

use crate::functions;
use crate::ports::ConnectionKind;
use engine_driver_kit::sql::{
    classify, tokens, Classification, Keyword, SqlLexOptions, SqlToken, SqlTokenKind,
};

/// Whether a statement only reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatementClass {
    Read,
    Write,
    /// Never allowed for an agent, whatever the access level (Redis commands
    /// the driver itself blocks).
    Blocked,
}

/// What the policy needs to know about a statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classified {
    /// Number of non-empty statements (`0` for empty or comments only).
    pub statements: usize,
    pub class: StatementClass,
    /// Why the statement is dangerous (needs explicit approval), if it is.
    pub danger: Option<String>,
    /// The statement is additive DDL (`ALTER TABLE ... ADD COLUMN`, `CREATE SCHEMA`) that the
    /// driver's own classifier would still ask to confirm: the user's approval of the write
    /// covers that question, so the layer sends it as confirmed.
    pub driver_confirms: bool,
    /// The first keyword or command verb, upper-cased (for messages).
    pub verb: String,
    /// The statement changes transaction or session state (`BEGIN`, `SET`,
    /// `USE`, `MULTI`, `SELECT` (Redis db), ...): each tool call is one
    /// self-contained statement, so agents may not run it at any level.
    pub session_state: bool,
    /// A clause agents may not send (ClickHouse `SETTINGS`): `agent.unsupported`.
    pub unsupported_clause: Option<String>,
    /// A function or clause that reaches beyond the tables (see
    /// [`crate::functions`]); refused at every level.
    pub blocked_function: Option<String>,
}

/// First words of SQL statements that change transaction or session state, or
/// hide the statement they run (`PREPARE` / `EXECUTE` / `DO`), or keep a
/// cursor open across calls.
pub const SQL_SESSION_WORDS: &[&str] = &[
    "BEGIN",
    "START",
    "COMMIT",
    "ROLLBACK",
    "END",
    "ABORT",
    "SAVEPOINT",
    "RELEASE",
    "SET",
    "RESET",
    "USE",
    "LOCK",
    "UNLOCK",
    "PRAGMA",
    "DISCARD",
    "PREPARE",
    "EXECUTE",
    "DEALLOCATE",
    "DO",
    "DECLARE",
    "FETCH",
    "MOVE",
    "CLOSE",
    "LISTEN",
    "UNLISTEN",
];

/// Redis commands that change the connection's state (transactions,
/// subscriptions, authentication, selected database, client settings) or
/// block it.
pub const REDIS_SESSION_VERBS: &[&str] = &[
    "MULTI",
    "EXEC",
    "DISCARD",
    "WATCH",
    "UNWATCH",
    "SELECT",
    "CLIENT",
    "SUBSCRIBE",
    "PSUBSCRIBE",
    "SSUBSCRIBE",
    "UNSUBSCRIBE",
    "PUNSUBSCRIBE",
    "SUNSUBSCRIBE",
    "MONITOR",
    "AUTH",
    "HELLO",
    "RESET",
    "QUIT",
    "READONLY",
    "READWRITE",
    "ASKING",
    "SYNC",
    "PSYNC",
    "REPLCONF",
    "WAIT",
    "WAITAOF",
    "BLPOP",
    "BRPOP",
    "BRPOPLPUSH",
    "BLMOVE",
    "BLMPOP",
    "BZPOPMIN",
    "BZPOPMAX",
    "BZMPOP",
];

/// Classifies SQL text for `kind` (whose lexer settings are `options`).
pub fn classify_sql(sql: &str, kind: ConnectionKind, options: &SqlLexOptions) -> Classified {
    let classification = classify(sql, options);
    let verb = classification
        .statements
        .first()
        .and_then(|statement| statement.keyword_range.clone())
        .map(|range| sql[range].to_ascii_uppercase())
        .unwrap_or_default();
    let (danger, driver_confirms) = agent_danger(sql, &classification, options);
    Classified {
        statements: classification.statements.len(),
        class: if classification.pageable {
            StatementClass::Read
        } else {
            StatementClass::Write
        },
        danger,
        driver_confirms,
        session_state: SQL_SESSION_WORDS.contains(&functions::first_word(sql, options).as_str()),
        unsupported_clause: functions::unsupported_clause(sql, kind, options),
        blocked_function: functions::blocked_function(sql, kind, options),
        verb,
    }
}

/// The first dangerous statement of the script, as the agent policy sees it, and whether an
/// additive statement before it needs the driver's confirmation.
///
/// The driver's classifier (which the app's own editor uses for its confirm dialogs) treats every
/// `ALTER` and `CREATE SCHEMA` as dangerous. For an agent, additive DDL is a normal write that the
/// user approves like an `INSERT`: `CREATE ...` (tables, indexes, views, sequences, schemas,
/// types), `COMMENT ON` and `ALTER TABLE ... ADD ...` (every clause an `ADD`). What destroys or
/// changes existing structure or data stays dangerous (`DROP`, `TRUNCATE`, `ALTER ... DROP /
/// ALTER / MODIFY / RENAME / SET ...`, `RENAME`, `CREATE OR REPLACE TABLE/DATABASE`, `DELETE` and
/// `UPDATE` without `WHERE`), and so does any `ALTER` form not recognised as additive.
fn agent_danger(
    sql: &str,
    classification: &Classification,
    options: &SqlLexOptions,
) -> (Option<String>, bool) {
    let mut scan = None;
    let mut confirms = false;
    for statement in &classification.statements {
        match (statement.keyword, statement.danger) {
            (Some(Keyword::Alter), Some(reason)) if reason.starts_with("ALTER") => {}
            (Some(Keyword::Create), Some(reason)) if reason.starts_with("CREATE DATABASE") => {}
            (Some(Keyword::Create), None) => {
                // The driver's classifier does not know `OR REPLACE TABLE`.
                let scan = scan.get_or_insert_with(|| tokens(sql, options));
                if Words::of(sql, scan.tokens.as_slice(), &statement.range).replaces_a_table() {
                    return (Some(OR_REPLACE_TABLE.to_owned()), false);
                }
                continue;
            }
            (Some(Keyword::Other), None)
                if statement
                    .keyword_range
                    .clone()
                    .is_some_and(|range| sql[range].eq_ignore_ascii_case("RENAME")) =>
            {
                return (Some(RENAME.to_owned()), false);
            }
            (_, Some(reason)) => return (Some(reason.to_owned()), false),
            (_, None) => continue,
        }
        // An ALTER or CREATE SCHEMA/DATABASE the driver asks to confirm: additive, or not?
        let scan = scan.get_or_insert_with(|| tokens(sql, options));
        // The content of an executable comment is code the lexer shows without its delimiters,
        // and a versioned comment can be read two ways: not worth reasoning about.
        if scan.executable_comment
            || scan.unterminated
            || classification.version_ambiguous
            || !Words::of(sql, scan.tokens.as_slice(), &statement.range).is_additive_ddl()
        {
            return (statement.danger.map(str::to_owned), false);
        }
        confirms = true;
    }
    // A versioned comment that cannot be read one way is dangerous even when no statement is.
    match &classification.danger {
        Some(danger) if classification.version_ambiguous => (Some(danger.reason.to_owned()), false),
        _ => (None, confirms),
    }
}

const OR_REPLACE_TABLE: &str =
    "CREATE OR REPLACE TABLE/DATABASE drops the existing object and its data";
const RENAME: &str = "RENAME changes the name of a database object, which breaks what uses the old name";

/// The tokens of one statement.
struct Words<'a> {
    sql: &'a str,
    tokens: &'a [SqlToken],
}

impl<'a> Words<'a> {
    fn of(sql: &'a str, all: &'a [SqlToken], range: &std::ops::Range<usize>) -> Self {
        let start = all.partition_point(|token| token.range.start < range.start);
        let length = all[start..].partition_point(|token| token.range.end <= range.end);
        Self {
            sql,
            tokens: &all[start..start + length],
        }
    }

    fn kind(&self, at: usize) -> Option<SqlTokenKind> {
        self.tokens.get(at).map(|token| token.kind)
    }

    /// The token at `at` is this bare word.
    fn is(&self, at: usize, word: &str) -> bool {
        self.tokens.get(at).is_some_and(|token| {
            token.kind == SqlTokenKind::Word && self.sql[token.range.clone()].eq_ignore_ascii_case(word)
        })
    }

    fn is_name(&self, at: usize) -> bool {
        matches!(self.kind(at), Some(SqlTokenKind::Word | SqlTokenKind::Quoted))
    }

    fn is_symbol(&self, at: usize, symbol: u8) -> bool {
        self.tokens.get(at).is_some_and(|token| {
            token.kind == SqlTokenKind::Other && self.sql.as_bytes()[token.range.start] == symbol
        })
    }

    /// `CREATE OR REPLACE [TEMPORARY] TABLE` (MariaDB): drops the table it replaces.
    fn replaces_a_table(&self) -> bool {
        self.is(0, "CREATE")
            && self.is(1, "OR")
            && self.is(2, "REPLACE")
            && (self.is(3, "TABLE") || (self.is(3, "TEMPORARY") && self.is(4, "TABLE")))
    }

    /// `CREATE SCHEMA|DATABASE` (not `OR REPLACE`), or `ALTER TABLE <name> ADD ... [, ADD ...]`.
    fn is_additive_ddl(&self) -> bool {
        if self.is(0, "CREATE") {
            return !(self.is(1, "OR") && self.is(2, "REPLACE"));
        }
        if !(self.is(0, "ALTER") && self.is(1, "TABLE")) {
            return false;
        }
        let mut at = 2;
        if self.is(at, "IF") && self.is(at + 1, "EXISTS") {
            at += 2;
        }
        if self.is(at, "ONLY") {
            at += 1;
        }
        // The table name, possibly `schema.table`.
        loop {
            if !self.is_name(at) {
                return false;
            }
            at += 1;
            if self.is_symbol(at, b'.') {
                at += 1;
            } else {
                break;
            }
        }
        if self.is_symbol(at, b'*') {
            at += 1;
        }
        // ClickHouse: ON CLUSTER <name>.
        if self.is(at, "ON") && self.is(at + 1, "CLUSTER") {
            if !self.is_name(at + 2) {
                return false;
            }
            at += 3;
        }
        // Every comma-separated clause (outside parentheses) is an ADD.
        let mut depth = 0usize;
        let mut starts_clause = true;
        for index in at..self.tokens.len() {
            if starts_clause {
                if !self.is(index, "ADD") {
                    return false;
                }
                starts_clause = false;
            }
            match self.tokens[index].kind {
                SqlTokenKind::Open => depth += 1,
                SqlTokenKind::Close => depth = depth.saturating_sub(1),
                SqlTokenKind::Comma if depth == 0 => starts_clause = true,
                _ => {}
            }
        }
        !starts_clause
    }
}

/// The leading keyword the lexer saw, for callers that refuse a statement
/// shape (`EXPLAIN` inside `explain_query`).
pub fn first_keyword(sql: &str, options: &SqlLexOptions) -> Option<Keyword> {
    classify(sql, options).statements.first()?.keyword
}

/// Verbs the driver lets through without confirmation (read-only).
pub const REDIS_READ_VERBS: &[&str] = &[
    "ASKING",
    "AUTH",
    "COMMAND",
    "DBSIZE",
    "ECHO",
    "EXISTS",
    "GET",
    "GETBIT",
    "GETRANGE",
    "HEXISTS",
    "HGET",
    "HGETALL",
    "HKEYS",
    "HLEN",
    "HMGET",
    "HRANDFIELD",
    "HSCAN",
    "HSTRLEN",
    "HVALS",
    "INFO",
    "LATENCY",
    "LINDEX",
    "LLEN",
    "LOLWUT",
    "LPOS",
    "LRANGE",
    "MEMORY",
    "MGET",
    "OBJECT",
    "PING",
    "PUBLISH",
    "PUBSUB",
    "RANDOMKEY",
    "SCAN",
    "SCARD",
    "SELECT",
    "SINTER",
    "SINTERCARD",
    "SISMEMBER",
    "SMEMBERS",
    "SMISMEMBER",
    "SORT_RO",
    "SSCAN",
    "STRLEN",
    "SUNION",
    "TIME",
    "TTL",
    "TYPE",
    "XINFO",
    "XLEN",
    "XRANGE",
    "XREAD",
    "XREVRANGE",
    "ZCARD",
    "ZCOUNT",
    "ZDIFF",
    "ZINTER",
    "ZLEXCOUNT",
    "ZMSCORE",
    "ZRANDMEMBER",
    "ZRANGE",
    "ZRANGEBYLEX",
    "ZRANGEBYSCORE",
    "ZRANK",
    "ZREVRANGE",
    "ZREVRANGEBYLEX",
    "ZREVRANGEBYSCORE",
    "ZREVRANK",
    "ZSCAN",
    "ZSCORE",
    "ZUNION",
];

/// Verbs the driver refuses outright.
pub const REDIS_BLOCKED_VERBS: &[&str] = &[
    "ACL",
    "CLIENT",
    "CLUSTER",
    "KEYS",
    "FLUSHALL",
    "SHUTDOWN",
    "CONFIG",
    "SAVE",
    "BGSAVE",
    "SLAVEOF",
    "REPLICAOF",
    "MIGRATE",
    "MODULE",
    "SCRIPT",
    "EVAL",
    "EVALSHA",
];

/// Verbs the driver would let through after a confirmation that an agent may
/// never run: they run server-side code (`FUNCTION`, `FCALL`, `*_RO` scripts),
/// crash or stall the server (`DEBUG`) or reconfigure replication / persistence.
pub const REDIS_AGENT_BLOCKED_EXTRA: &[&str] = &[
    "DEBUG",
    "FUNCTION",
    "FCALL",
    "FCALL_RO",
    "EVAL_RO",
    "EVALSHA_RO",
    "FAILOVER",
    "BGREWRITEAOF",
];

/// Writes that destroy more than the keys they name: dangerous, so they need
/// `dangerous=true` and the user's approval (`FLUSH*` is dangerous by prefix).
pub const REDIS_DANGEROUS_VERBS: &[&str] = &["SWAPDB"];

/// Read verbs of the driver that an agent must not run without write access:
/// they change something outside the keyspace (messages to subscribers, the
/// connection's authentication).
pub const REDIS_AGENT_WRITE_OVERRIDES: &[&str] = &["PUBLISH", "AUTH"];

/// Splits a command the way the driver does (whitespace, `"` quotes, `\`
/// escapes inside quotes). `None` for unbalanced quoting.
pub fn parse_redis_args(command: &str) -> Option<Vec<String>> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut escaped = false;
    for ch in command.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_quotes => escaped = true,
            '"' => in_quotes = !in_quotes,
            ch if ch.is_whitespace() && !in_quotes => {
                if !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }
    if escaped || in_quotes {
        return None;
    }
    if !current.is_empty() {
        args.push(current);
    }
    Some(args)
}

/// Classifies one Redis command line. An unparsable or unknown command is a
/// write; a blank one is a (harmless) read.
pub fn classify_redis(command: &str) -> Classified {
    let verb = parse_redis_args(command)
        .and_then(|args| args.into_iter().next())
        .unwrap_or_default()
        .to_ascii_uppercase();
    let parsable = parse_redis_args(command).is_some();
    let class = if !parsable {
        StatementClass::Write
    } else if verb.is_empty() {
        StatementClass::Read
    } else if REDIS_BLOCKED_VERBS.contains(&verb.as_str())
        || REDIS_AGENT_BLOCKED_EXTRA.contains(&verb.as_str())
    {
        StatementClass::Blocked
    } else if REDIS_READ_VERBS.contains(&verb.as_str())
        && !REDIS_AGENT_WRITE_OVERRIDES.contains(&verb.as_str())
    {
        StatementClass::Read
    } else {
        StatementClass::Write
    };
    let dangerous = verb.starts_with("FLUSH") || REDIS_DANGEROUS_VERBS.contains(&verb.as_str());
    let danger = (class == StatementClass::Write && dangerous)
        .then(|| format!("{verb} destroys keys beyond the ones it names"));
    Classified {
        statements: usize::from(!verb.is_empty()),
        class,
        danger,
        driver_confirms: false,
        session_state: REDIS_SESSION_VERBS.contains(&verb.as_str()),
        unsupported_clause: None,
        blocked_function: None,
        verb,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pg(sql: &str) -> Classified {
        classify_sql(sql, ConnectionKind::PostgreSql, &SqlLexOptions::postgres())
    }

    #[test]
    fn plain_reads_are_reads() {
        for sql in [
            "SELECT 1",
            "  select * from t where a = 1  ",
            "-- note\nSELECT 1",
            "/* head */ SELECT 1 /* tail */",
            "WITH c AS (SELECT 1) SELECT * FROM c",
            "VALUES (1), (2)",
            "TABLE t",
            "SHOW TABLES",
            "EXPLAIN SELECT 1",
            "EXPLAIN DELETE FROM t",
            "(SELECT 1) UNION (SELECT 2)",
            "SELECT 'a;b' AS x",
            "SELECT 1 -- ; DROP TABLE t",
            "SELECT 1 /* ; DROP TABLE t */",
            "SELECT $$ ; DROP TABLE t $$",
        ] {
            let got = pg(sql);
            assert_eq!(got.class, StatementClass::Read, "{sql}");
            assert_eq!(got.statements, 1, "{sql}");
            assert_eq!(got.danger, None, "{sql}");
        }
    }

    #[test]
    fn writes_and_unknown_statements_are_writes() {
        for sql in [
            "INSERT INTO t VALUES (1)",
            "UPDATE t SET a = 1 WHERE id = 2",
            "DELETE FROM t WHERE id = 1",
            "CREATE TABLE t (a int)",
            "DROP TABLE t",
            "ALTER TABLE t ADD c int",
            "TRUNCATE t",
            "GRANT ALL ON t TO u",
            "COPY t TO '/tmp/x'",
            "CALL p()",
            "VACUUM",
            "SET search_path = x",
            "USE other",
            "PRAGMA user_version = 3",
            "BEGIN",
            "something the lexer does not know",
            "",
        ] {
            let got = pg(sql);
            if sql.is_empty() {
                assert_eq!(got.statements, 0);
            } else {
                assert_eq!(got.class, StatementClass::Write, "{sql}");
            }
        }
    }

    #[test]
    fn a_write_hidden_behind_a_read_is_a_write() {
        for sql in [
            "WITH d AS (DELETE FROM t RETURNING *) SELECT * FROM d",
            "WITH d AS (INSERT INTO t VALUES (1) RETURNING *) SELECT * FROM d",
            "WITH d AS (UPDATE t SET a = 1 RETURNING *) SELECT * FROM d",
            "WITH x AS (SELECT 1) DELETE FROM t",
            "SELECT * INTO copy FROM t",
            "SELECT * FROM t INTO OUTFILE '/tmp/x'",
            "EXPLAIN ANALYZE UPDATE t SET a = 1",
            "EXPLAIN ANALYZE DELETE FROM t WHERE a = 1",
        ] {
            assert_eq!(pg(sql).class, StatementClass::Write, "{sql}");
        }
        assert_eq!(
            classify_sql(
                "ANALYZE SELECT 1",
                ConnectionKind::MariaDb,
                &SqlLexOptions::mariadb()
            )
            .class,
            StatementClass::Write
        );
    }

    #[test]
    fn multiple_statements_are_counted_outside_strings_and_comments() {
        assert_eq!(pg("SELECT 1; SELECT 2").statements, 2);
        assert_eq!(pg("SELECT 1;").statements, 1);
        assert_eq!(pg("SELECT 1; -- trailing").statements, 1);
        assert_eq!(pg("SELECT ';'; SELECT ';'").statements, 2);
        assert_eq!(pg("SELECT ';' -- ; \n").statements, 1);
        assert_eq!(pg("SELECT 1 /* ; */").statements, 1);
        assert_eq!(pg("SELECT 'it''s; fine'").statements, 1);
        assert_eq!(pg("SELECT \"a;b\" FROM t").statements, 1);
        // A script of reads is still not a read: the policy refuses it by count.
        assert_eq!(pg("SELECT 1; DROP TABLE t").statements, 2);
        // MySQL: a backslash escapes the quote, so the `;` stays inside the string.
        let mysql = SqlLexOptions::mysql();
        assert_eq!(
            classify_sql("SELECT 'a\\';b'", ConnectionKind::MySql, &mysql).statements,
            1
        );
        assert_eq!(
            classify_sql(
                "SELECT 'a\\'; DROP TABLE t; --'",
                ConnectionKind::MySql,
                &mysql
            )
            .statements,
            1
        );
    }

    #[test]
    fn executable_comments_and_quoting_dialects_cannot_hide_a_write() {
        let mysql = SqlLexOptions::mysql();
        // MySQL runs the content of `/*! ... */`.
        assert_eq!(
            classify_sql("/*!50000 DROP TABLE t */", ConnectionKind::MySql, &mysql).class,
            StatementClass::Write
        );
        assert!(
            classify_sql("/*!50000 DROP TABLE t */", ConnectionKind::MySql, &mysql)
                .danger
                .is_some()
        );
        assert_eq!(
            classify_sql(
                "SELECT 1 /*!50000 ; DROP TABLE t */",
                ConnectionKind::MySql,
                &mysql
            )
            .statements,
            2
        );
        // A `--` comment needs a space in MySQL: `--1` is arithmetic, not a comment.
        assert_eq!(
            classify_sql("SELECT 1--1; DROP TABLE t", ConnectionKind::MySql, &mysql).statements,
            2
        );
        // A `#` comment hides the rest of the line in MySQL only.
        assert_eq!(
            classify_sql("SELECT 1 # ; DROP TABLE t", ConnectionKind::MySql, &mysql).statements,
            1
        );
        assert_eq!(pg("SELECT 1 # 2; DROP TABLE t").statements, 2);
        // ANSI quotes: `"` is a string in MySQL, an identifier in PostgreSQL.
        assert_eq!(
            classify_sql("SELECT \"a;b\"", ConnectionKind::MySql, &mysql).statements,
            1
        );
        // PostgreSQL dollar quotes hide a `;`; SQLite does not have them.
        assert_eq!(pg("SELECT $a$ ; DROP TABLE t $a$").statements, 1);
        assert_eq!(
            classify_sql(
                "SELECT $$ ; DROP TABLE t $$",
                ConnectionKind::Sqlite,
                &SqlLexOptions::sqlite()
            )
            .statements,
            2
        );
    }

    #[test]
    fn danger_follows_the_driver_classifier() {
        assert!(pg("DROP TABLE t").danger.is_some());
        assert!(pg("TRUNCATE t").danger.is_some());
        assert!(pg("DELETE FROM t").danger.is_some());
        assert!(pg("UPDATE t SET a = 1").danger.is_some());
        assert!(pg("ALTER TABLE t DROP c").danger.is_some());
        assert!(pg("DELETE FROM t WHERE id = 1").danger.is_none());
        assert!(pg("UPDATE t SET a = 1 WHERE id = 1").danger.is_none());
        assert!(pg("INSERT INTO t VALUES (1)").danger.is_none());
        assert!(pg("SELECT * FROM t").danger.is_none());
        // A dangerous statement later in a script is reported too.
        assert!(pg("SELECT 1; DROP TABLE t").danger.is_some());
    }

    fn dialect(kind: ConnectionKind) -> SqlLexOptions {
        match kind {
            ConnectionKind::PostgreSql => SqlLexOptions::postgres(),
            ConnectionKind::MySql => SqlLexOptions::mysql(),
            ConnectionKind::MariaDb => SqlLexOptions::mariadb(),
            ConnectionKind::Sqlite => SqlLexOptions::sqlite(),
            ConnectionKind::ClickHouse => SqlLexOptions::clickhouse(),
            other => panic!("{other:?} is not SQL"),
        }
    }

    /// `(dialect, statement)`: additive DDL is a normal write for an agent, and the driver's own
    /// classifier is asked to confirm it only when it flags it.
    const ADDITIVE: &[(ConnectionKind, &str)] = &[
        (ConnectionKind::PostgreSql, "CREATE TABLE t (a int)"),
        (ConnectionKind::PostgreSql, "CREATE TABLE IF NOT EXISTS t (a int)"),
        (ConnectionKind::PostgreSql, "CREATE INDEX i ON t (a)"),
        (ConnectionKind::PostgreSql, "CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS i ON t (a)"),
        (ConnectionKind::PostgreSql, "CREATE VIEW v AS SELECT 1"),
        (ConnectionKind::PostgreSql, "CREATE OR REPLACE VIEW v AS SELECT 1"),
        (ConnectionKind::PostgreSql, "CREATE SEQUENCE s"),
        (ConnectionKind::PostgreSql, "CREATE SCHEMA IF NOT EXISTS s"),
        (ConnectionKind::PostgreSql, "CREATE TYPE mood AS ENUM ('a', 'b')"),
        (ConnectionKind::PostgreSql, "COMMENT ON TABLE t IS 'x'"),
        (ConnectionKind::PostgreSql, "ALTER TABLE t ADD c int"),
        (ConnectionKind::PostgreSql, "ALTER TABLE t ADD COLUMN c int"),
        (ConnectionKind::PostgreSql, "alter table public.t add column if not exists c numeric(10, 2) default 0"),
        (ConnectionKind::PostgreSql, "ALTER TABLE IF EXISTS ONLY \"my schema\".\"t\" ADD COLUMN c int"),
        (ConnectionKind::PostgreSql, "ALTER TABLE t ADD COLUMN a int, ADD COLUMN b text"),
        (ConnectionKind::PostgreSql, "ALTER TABLE t ADD CONSTRAINT u UNIQUE (a, b)"),
        (ConnectionKind::PostgreSql, "ALTER TABLE t ADD CONSTRAINT fk FOREIGN KEY (a) REFERENCES o (id)"),
        (ConnectionKind::PostgreSql, "/* migration */ ALTER TABLE t ADD COLUMN biz_domain text -- note"),
        (ConnectionKind::MySql, "CREATE TABLE t (a int)"),
        (ConnectionKind::MySql, "CREATE INDEX i ON t (a)"),
        (ConnectionKind::MySql, "CREATE SCHEMA s"),
        (ConnectionKind::MySql, "CREATE DATABASE IF NOT EXISTS d"),
        (ConnectionKind::MySql, "ALTER TABLE `t` ADD COLUMN `c` int AFTER `a`"),
        (ConnectionKind::MySql, "ALTER TABLE t ADD INDEX i (a)"),
        (ConnectionKind::MySql, "ALTER TABLE t ADD KEY k (a), ADD UNIQUE (b)"),
        (ConnectionKind::MySql, "ALTER TABLE t ADD (a int, b int)"),
        (ConnectionKind::MySql, "ALTER TABLE t ADD COLUMN c enum('a,b', 'c') NOT NULL DEFAULT 'a,b'"),
        (ConnectionKind::MariaDb, "ALTER TABLE t ADD COLUMN IF NOT EXISTS c int"),
        (ConnectionKind::MariaDb, "CREATE DATABASE d"),
        (ConnectionKind::Sqlite, "CREATE TABLE t (a int)"),
        (ConnectionKind::Sqlite, "CREATE INDEX IF NOT EXISTS i ON t (a)"),
        (ConnectionKind::Sqlite, "ALTER TABLE t ADD COLUMN c int"),
        (ConnectionKind::ClickHouse, "CREATE TABLE t (a Int32) ENGINE = MergeTree ORDER BY a"),
        (ConnectionKind::ClickHouse, "ALTER TABLE t ADD COLUMN IF NOT EXISTS c Int32"),
        (ConnectionKind::ClickHouse, "ALTER TABLE db.t ON CLUSTER main ADD COLUMN c String AFTER a"),
        (ConnectionKind::ClickHouse, "ALTER TABLE t ADD INDEX i a TYPE minmax GRANULARITY 1"),
    ];

    /// Destructive DDL, and what cannot be told to be additive, stay dangerous.
    const DESTRUCTIVE: &[(ConnectionKind, &str)] = &[
        (ConnectionKind::PostgreSql, "DROP TABLE t"),
        (ConnectionKind::PostgreSql, "DROP INDEX i"),
        (ConnectionKind::PostgreSql, "DROP INDEX CONCURRENTLY i"),
        (ConnectionKind::PostgreSql, "DROP SCHEMA s CASCADE"),
        (ConnectionKind::PostgreSql, "TRUNCATE t"),
        (ConnectionKind::PostgreSql, "DELETE FROM t"),
        (ConnectionKind::PostgreSql, "UPDATE t SET a = 1"),
        (ConnectionKind::PostgreSql, "ALTER TABLE t DROP COLUMN c"),
        (ConnectionKind::PostgreSql, "ALTER TABLE t DROP CONSTRAINT c"),
        (ConnectionKind::PostgreSql, "ALTER TABLE t ALTER COLUMN c TYPE text"),
        (ConnectionKind::PostgreSql, "ALTER TABLE t ALTER COLUMN c SET NOT NULL"),
        (ConnectionKind::PostgreSql, "ALTER TABLE t RENAME TO u"),
        (ConnectionKind::PostgreSql, "ALTER TABLE t RENAME COLUMN a TO b"),
        (ConnectionKind::PostgreSql, "ALTER TABLE t SET SCHEMA other"),
        (ConnectionKind::PostgreSql, "ALTER TABLE t SET (fillfactor = 70)"),
        (ConnectionKind::PostgreSql, "ALTER TABLE t SET TABLESPACE x"),
        (ConnectionKind::PostgreSql, "ALTER TABLE t ADD COLUMN a int, DROP COLUMN b"),
        (ConnectionKind::PostgreSql, "ALTER TABLE t DROP COLUMN b, ADD COLUMN a int"),
        (ConnectionKind::PostgreSql, "ALTER TABLE t ADD COLUMN a int,"),
        (ConnectionKind::PostgreSql, "ALTER TABLE t"),
        (ConnectionKind::PostgreSql, "ALTER TABLE t ATTACH PARTITION p FOR VALUES IN (1)"),
        (ConnectionKind::PostgreSql, "ALTER INDEX i RENAME TO j"),
        (ConnectionKind::PostgreSql, "ALTER SEQUENCE s RESTART"),
        (ConnectionKind::PostgreSql, "ALTER VIEW v RENAME TO w"),
        (ConnectionKind::PostgreSql, "ALTER ROLE r SUPERUSER"),
        (ConnectionKind::PostgreSql, "ALTER SYSTEM SET x = 1"),
        (ConnectionKind::PostgreSql, "ALTER TABLE ADD COLUMN c int"),
        (ConnectionKind::PostgreSql, "GRANT ALL ON t TO u"),
        (ConnectionKind::PostgreSql, "ALTER TABLE t RENAME TO \"ADD\""),
        (ConnectionKind::MySql, "DROP DATABASE d"),
        (ConnectionKind::MySql, "ALTER TABLE t DROP COLUMN c"),
        (ConnectionKind::MySql, "ALTER TABLE t DROP INDEX i"),
        (ConnectionKind::MySql, "ALTER TABLE t MODIFY COLUMN c bigint"),
        (ConnectionKind::MySql, "ALTER TABLE t CHANGE a b int"),
        (ConnectionKind::MySql, "ALTER TABLE t ADD COLUMN c int, MODIFY c bigint"),
        (ConnectionKind::MySql, "ALTER TABLE t ADD COLUMN c int, ALGORITHM = INPLACE"),
        (ConnectionKind::MySql, "ALTER TABLE t ENGINE = MyISAM"),
        (ConnectionKind::MySql, "ALTER ONLINE TABLE t ADD COLUMN c int"),
        (ConnectionKind::MySql, "RENAME TABLE a TO b"),
        (ConnectionKind::MySql, "rename table a to b, c to d"),
        (ConnectionKind::MySql, "ALTER TABLE t RENAME TO u"),
        (ConnectionKind::MySql, "/*!50000 DROP TABLE t */"),
        (ConnectionKind::MySql, "ALTER TABLE t /*!50000 DROP COLUMN c */"),
        (ConnectionKind::MariaDb, "CREATE OR REPLACE TABLE t (a int)"),
        (ConnectionKind::MariaDb, "CREATE OR REPLACE TEMPORARY TABLE t (a int)"),
        (ConnectionKind::MariaDb, "CREATE OR REPLACE DATABASE d"),
        (ConnectionKind::MariaDb, "CREATE OR REPLACE SCHEMA d"),
        (ConnectionKind::Sqlite, "DROP TABLE t"),
        (ConnectionKind::Sqlite, "ALTER TABLE t DROP COLUMN c"),
        (ConnectionKind::Sqlite, "ALTER TABLE t RENAME TO u"),
        (ConnectionKind::Sqlite, "ALTER TABLE t RENAME COLUMN a TO b"),
        (ConnectionKind::ClickHouse, "DROP TABLE t"),
        (ConnectionKind::ClickHouse, "TRUNCATE TABLE t"),
        (ConnectionKind::ClickHouse, "ALTER TABLE t DELETE WHERE a = 1"),
        (ConnectionKind::ClickHouse, "ALTER TABLE t UPDATE a = 1 WHERE b = 2"),
        (ConnectionKind::ClickHouse, "ALTER TABLE t DROP COLUMN c"),
        (ConnectionKind::ClickHouse, "ALTER TABLE t MODIFY COLUMN c String"),
        (ConnectionKind::ClickHouse, "ALTER TABLE t ON CLUSTER main DROP PARTITION 1"),
        (ConnectionKind::ClickHouse, "ALTER TABLE t ADD COLUMN c Int32, DELETE WHERE a = 1"),
        (ConnectionKind::ClickHouse, "RENAME TABLE a TO b"),
    ];

    #[test]
    fn additive_ddl_is_a_plain_write_for_an_agent() {
        for (kind, sql) in ADDITIVE {
            let got = classify_sql(sql, *kind, &dialect(*kind));
            assert_eq!(got.class, StatementClass::Write, "{kind:?}: {sql}");
            assert_eq!(got.danger, None, "{kind:?}: {sql}");
            assert_eq!(got.statements, 1, "{kind:?}: {sql}");
            assert!(!got.session_state, "{kind:?}: {sql}");
            // The driver's own check still flags ALTER and CREATE SCHEMA: the approval covers it.
            let driver = engine_driver_kit::sql::classify(sql, &dialect(*kind));
            assert_eq!(got.driver_confirms, driver.danger.is_some(), "{kind:?}: {sql}");
        }
    }

    #[test]
    fn destructive_ddl_and_unknown_alter_forms_stay_dangerous() {
        for (kind, sql) in DESTRUCTIVE {
            let got = classify_sql(sql, *kind, &dialect(*kind));
            assert_eq!(got.class, StatementClass::Write, "{kind:?}: {sql}");
            assert!(got.danger.is_some(), "{kind:?}: {sql} must be dangerous");
            assert!(!got.driver_confirms, "{kind:?}: {sql}");
        }
    }

    #[test]
    fn danger_is_judged_per_statement_of_a_script() {
        let batch = |sql: &str| classify_sql(sql, ConnectionKind::PostgreSql, &SqlLexOptions::postgres());
        // Additive statements around a destructive one: the destructive one is reported.
        let got = batch("ALTER TABLE t ADD COLUMN c int; ALTER TABLE t DROP COLUMN c");
        assert_eq!(got.statements, 2);
        assert!(got.danger.as_deref().is_some_and(|reason| reason.starts_with("ALTER")));
        assert!(batch("CREATE TABLE t (a int); DROP TABLE t").danger.is_some());
        assert!(batch("ALTER TABLE t ADD c int; DELETE FROM t").danger.is_some());
        assert_eq!(batch("ALTER TABLE t ADD c int; CREATE INDEX i ON t (c)").danger, None);
        // A reading that does not close its comment, or a statement that merely mentions the words.
        assert!(batch("ALTER TABLE t ADD c int /* DROP COLUMN").danger.is_some());
        assert_eq!(batch("ALTER TABLE t ADD c text DEFAULT 'a, DROP COLUMN b'").danger, None);
        assert_eq!(batch("ALTER TABLE t ADD c int -- , DROP COLUMN b").danger, None);
        assert!(batch("ALTER TABLE t ADD c int, -- x\n DROP COLUMN b").danger.is_some());
    }

    #[test]
    fn a_statement_that_hides_in_a_versioned_comment_stays_dangerous() {
        let mysql = SqlLexOptions::mysql();
        let got = classify_sql(
            "/*!50000 ALTER TABLE t ADD COLUMN c int */",
            ConnectionKind::MySql,
            &mysql,
        );
        assert!(got.danger.is_some());
        assert!(!got.driver_confirms);
    }

    #[test]
    fn verbs_are_reported_upper_case() {
        assert_eq!(pg("  insert into t values (1)").verb, "INSERT");
        assert_eq!(pg("/* x */ select 1").verb, "SELECT");
        assert_eq!(
            first_keyword("EXPLAIN SELECT 1", &SqlLexOptions::postgres()),
            Some(Keyword::Explain)
        );
    }

    #[test]
    fn redis_commands_are_classified_by_verb() {
        for read in [
            "GET k",
            "get k",
            "  HGETALL h",
            "SCAN 0 MATCH a*",
            "INFO server",
            "PING",
            "\"get\" k",
        ] {
            assert_eq!(classify_redis(read).class, StatementClass::Read, "{read}");
        }
        for write in [
            "SET k v",
            "DEL k",
            "HSET h f v",
            "EXPIRE k 10",
            "PUBLISH c m",
            "AUTH pw",
            "FLUSHDB",
            "SWAPDB 0 1",
            "UNLINK k",
            "MADEUP",
        ] {
            assert_eq!(
                classify_redis(write).class,
                StatementClass::Write,
                "{write}"
            );
        }
        for blocked in [
            "KEYS *",
            "FLUSHALL",
            "CONFIG SET x y",
            "EVAL 1 0",
            "SHUTDOWN",
            "ACL LIST",
            "script flush",
            "DEBUG SLEEP 0",
            "fcall f 0",
            "FUNCTION LOAD x",
        ] {
            assert_eq!(
                classify_redis(blocked).class,
                StatementClass::Blocked,
                "{blocked}"
            );
        }
        assert_eq!(
            classify_redis("GET \"unterminated").class,
            StatementClass::Write
        );
        assert_eq!(
            classify_redis("FLUSHDB")
                .danger
                .as_deref()
                .map(|d| d.starts_with("FLUSHDB")),
            Some(true)
        );
        assert!(classify_redis("flushdb async").danger.is_some());
        assert!(classify_redis("swapdb 0 1").danger.is_some());
        assert!(classify_redis("DEL a").danger.is_none());
        assert!(classify_redis("SET k v").danger.is_none());
        assert_eq!(classify_redis("GET k").verb, "GET");
    }

    #[test]
    fn transaction_and_session_state_is_flagged_for_every_dialect() {
        for sql in [
            "BEGIN",
            "begin transaction",
            "START TRANSACTION READ ONLY",
            "COMMIT",
            "ROLLBACK",
            "ROLLBACK TO SAVEPOINT s",
            "END",
            "SAVEPOINT s",
            "RELEASE SAVEPOINT s",
            "SET search_path = x",
            "SET ROLE admin",
            "SET SESSION AUTHORIZATION bob",
            "RESET ALL",
            "USE other",
            "LOCK TABLE t",
            "UNLOCK TABLES",
            "PRAGMA writable_schema = ON",
            "PRAGMA table_info(t)",
            "DISCARD ALL",
            "PREPARE s AS SELECT 1",
            "EXECUTE s",
            "DEALLOCATE s",
            "DO $$ BEGIN END $$",
            "DECLARE c CURSOR FOR SELECT 1",
            "FETCH 5 FROM c",
            "  /* c */ -- x\n set role admin",
        ] {
            assert!(pg(sql).session_state, "{sql}");
        }
        for sql in [
            "SELECT 1",
            "WITH c AS (SELECT 1) SELECT * FROM c",
            "SELECT set_config FROM t",
            "SELECT 'BEGIN'",
            "SHOW TABLES",
            "EXPLAIN SELECT 1",
            "INSERT INTO t VALUES (1)",
            "UPDATE t SET a = 1 WHERE id = 1",
        ] {
            assert!(!pg(sql).session_state, "{sql}");
        }
    }

    #[test]
    fn redis_session_commands_are_flagged() {
        for command in [
            "MULTI",
            "EXEC",
            "DISCARD",
            "WATCH k",
            "UNWATCH",
            "SELECT 3",
            "CLIENT SETNAME x",
            "SUBSCRIBE c",
            "PSUBSCRIBE c*",
            "SSUBSCRIBE c",
            "UNSUBSCRIBE",
            "MONITOR",
            "AUTH pw",
            "HELLO 3",
            "RESET",
            "QUIT",
            "BLPOP k 0",
            "\"multi\"",
            "select 1",
        ] {
            assert!(classify_redis(command).session_state, "{command}");
        }
        for command in [
            "GET k",
            "SET k v",
            "SCAN 0",
            "FLUSHDB",
            "EXPIRE k 1",
            "SORT_RO k",
        ] {
            assert!(!classify_redis(command).session_state, "{command}");
        }
    }

    #[test]
    fn blocked_functions_are_reported_with_the_classification() {
        let got = pg("SELECT pg_read_file('/etc/passwd')");
        assert_eq!(got.blocked_function.as_deref(), Some("pg_read_file"));
        assert_eq!(got.class, StatementClass::Read);
        assert_eq!(pg("SELECT 1").blocked_function, None);
        assert_eq!(classify_redis("GET k").blocked_function, None);
    }

    #[test]
    fn redis_commands_that_read_but_also_write_are_writes_individually() {
        for command in [
            "GETDEL k",
            "GETEX k EX 10",
            "SORT k STORE dest",
            "SORT k BY w GET * STORE dest",
            "BITFIELD k SET u8 0 1",
            "BITFIELD k INCRBY u8 0 1",
            "XREADGROUP GROUP g c STREAMS s >",
            "GEORADIUS k 0 0 10 km STORE dest",
            "GEOSEARCHSTORE dest src FROMLONLAT 0 0 BYRADIUS 1 km",
            "SPOP k",
            "LPOP k",
            "RPOPLPUSH a b",
            "SETRANGE k 0 x",
            "APPEND k x",
            "INCR k",
            "EXPIRE k 1",
            "COPY a b",
            "RENAME a b",
            "MOVE k 1",
            "PERSIST k",
            "SLOWLOG RESET",
        ] {
            let got = classify_redis(command);
            assert_eq!(got.class, StatementClass::Write, "{command}");
        }
    }

    #[test]
    fn mysql_select_into_a_variable_is_a_write_fail_closed() {
        let mysql = SqlLexOptions::mysql();
        for sql in [
            "SELECT 1 INTO @x",
            "SELECT a, b FROM t INTO @x, @y",
            "SELECT 1 INTO OUTFILE '/tmp/x'",
        ] {
            let got = classify_sql(sql, ConnectionKind::MySql, &mysql);
            assert_eq!(got.class, StatementClass::Write, "{sql}");
        }
    }
}
