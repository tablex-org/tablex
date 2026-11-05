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
use engine_driver_kit::sql::{classify, Keyword, SqlLexOptions};

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
    Classified {
        statements: classification.statements.len(),
        class: if classification.pageable {
            StatementClass::Read
        } else {
            StatementClass::Write
        },
        danger: classification.danger.map(|danger| danger.reason.to_owned()),
        session_state: SQL_SESSION_WORDS.contains(&functions::first_word(sql, options).as_str()),
        unsupported_clause: functions::unsupported_clause(sql, kind, options),
        blocked_function: functions::blocked_function(sql, kind, options),
        verb,
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
        assert!(pg("ALTER TABLE t ADD c int").danger.is_some());
        assert!(pg("DELETE FROM t WHERE id = 1").danger.is_none());
        assert!(pg("UPDATE t SET a = 1 WHERE id = 1").danger.is_none());
        assert!(pg("INSERT INTO t VALUES (1)").danger.is_none());
        assert!(pg("SELECT * FROM t").danger.is_none());
        // A dangerous statement later in a script is reported too.
        assert!(pg("SELECT 1; DROP TABLE t").danger.is_some());
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
