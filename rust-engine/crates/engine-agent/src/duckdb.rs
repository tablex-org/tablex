//! What an agent may never run on a DuckDB connection, whatever its access level.
//!
//! DuckDB reads and writes local files and URLs from plain SQL: table functions (`read_csv`,
//! `read_text`, `glob`, ...), `COPY`, a string or a quoted name in the place of a table
//! (`FROM '/path/x.csv'`, `FROM "x.csv"`: a replacement scan), `ATTACH` of another database file,
//! extensions (`INSTALL`, `LOAD`) and secrets. An agent must not read the user's files or send
//! them away with these, so every one of them is refused here.
//!
//! **This is the second layer.** The first is the engine: tablex opens the DuckDB instance of an
//! agent session with `enable_external_access = false` and a locked configuration, which DuckDB
//! enforces for everything below, including what a denylist forgets. This scan stops the text
//! before it reaches the engine (a clear message instead of an engine error). It is not a way to
//! make an instance that could not be locked safe: DuckDB's grammar keeps growing places where a
//! string is a file, and a denylist over it keeps missing some. The tool layer therefore refuses
//! every statement tool on a session whose instance has file access on (`SessionInfo::
//! file_access_locked`; the app shares its instance with the user's own connection), and this
//! scan is defence in depth behind that.
//!
//! The text is read with the same lexer as the driver (`engine_driver_kit::sql`,
//! `SqlLexOptions::duckdb()`: this build's only parser is DuckDB's own PEG one, and its tokenizer
//! is mirrored there), never strings, comments or dollar-quoted text.

use crate::sanitize::{clean_name, MAX_NAME_CHARS};
use engine_driver_kit::sql::{tokens, SqlLexOptions, SqlToken, SqlTokenKind};

/// Function names that read or write files and URLs, run SQL text given as a string, open
/// another database or give the secrets away; `*` matches any run of characters.
///
/// * `read_*`: `read_csv`, `read_csv_auto`, `read_parquet`, `read_json`, `read_json_auto`,
///   `read_ndjson`, `read_text`, `read_blob`, `read_xlsx`, ...
/// * `*_scan`: `parquet_scan`, `delta_scan`, `iceberg_scan`, `sqlite_scan`, `postgres_scan`, ...
/// * `parquet_*`: `parquet_metadata`, `parquet_schema`, `parquet_file_metadata`, ...
/// * `glob`, `sniff_csv`: list or probe files.
/// * `query`, `query_table`: run a statement, or read a table, whose text is a string.
/// * `getenv`, `duckdb_secrets`, `which_secret`: the environment and the stored credentials.
///
/// * `json_serialize_sql`, `json_execute_serialized_sql`: SQL text carried in a string or JSON, out
///   of the scan's sight, and the second one runs it on a connection of its own (outside a
///   `READ ONLY` transaction).
/// * `enable_logging`, `disable_logging`, `enable_profiling`, `disable_profiling`: write log and
///   profile files.
const FUNCTIONS: &[&str] = &[
    "json_serialize_sql",
    "json_deserialize_sql",
    "json_execute_serialized_sql",
    "*_logging",
    "*_profiling",
    "read_*",
    "*_scan",
    "parquet_*",
    "glob",
    "sniff_csv",
    "iceberg_*",
    "delta_*",
    "ducklake_*",
    "st_read*",
    "postgres_*",
    "mysql_*",
    "sqlite_*",
    "query",
    "query_table",
    "getenv",
    "duckdb_secrets",
    "which_secret",
];

/// Statements an agent never runs, by their first word (after `EXPLAIN [ANALYZE]`):
/// `COPY` and `EXPORT` / `IMPORT DATABASE` move data to and from files, `ATTACH` and `CONNECT` open
/// another database file or server (or an external resource), `DETACH` and `DISCONNECT` close one,
/// `INSTALL` and `LOAD` fetch and load extensions, `REGISTER` and `DESTROY` (`EXTERNAL RESOURCE`)
/// manage external resources.
const VERBS: &[&str] = &[
    "COPY", "EXPORT", "IMPORT", "ATTACH", "DETACH", "CONNECT", "DISCONNECT", "INSTALL", "LOAD", "REGISTER",
    "DESTROY",
];

/// What `CREATE | DROP | ALTER [OR REPLACE] [TEMPORARY | PERSISTENT]` may not be followed by:
/// `SECRET`, `EXTENSION [REPOSITORY]`, `EXTERNAL RESOURCE`.
const CREATE_KINDS: &[&str] = &["SECRET", "EXTENSION", "EXTERNAL"];

/// `PRAGMA`, `SET`, `RESET` and `CALL` that name a setting or function writing log or profile
/// files (`PRAGMA enable_profiling = 'json'`, `SET profiling_output = '...'`, `SET log_query_path`).
const OUTPUT_SETTINGS: &[&str] = &["*profil*", "*logging*", "log_*"];

/// The last part of a dotted name that DuckDB tries as a file (`FROM data.csv`, `"a"."csv"`) when
/// no table has that name.
const DATA_EXTENSIONS: &[&str] = &[
    "csv", "tsv", "json", "ndjson", "jsonl", "parquet", "txt", "gz", "zst", "xlsx", "arrow", "tbl",
];

/// Words that start a table reference: what follows may be a file.
const TABLE_WORDS: &[&str] = &[
    "FROM", "JOIN", "DESCRIBE", "SUMMARIZE", "PIVOT", "UNPIVOT", "TABLE", "SHOW", "PIVOT_WIDER", "PIVOT_LONGER",
    "USING",
];

/// Words that end a `FROM` list at its own depth: a comma after them is not a table list's.
const ENDS_FROM_LIST: &[&str] = &[
    "WHERE", "GROUP", "HAVING", "ORDER", "LIMIT", "OFFSET", "UNION", "INTERSECT", "EXCEPT", "WINDOW",
    "QUALIFY", "SELECT", "RETURNING", "VALUES", "SET", "FETCH",
];

/// Functions whose parentheses hold `x FROM y` with an expression after `FROM`, not a table.
const FROM_IN_EXPRESSION: &[&str] = &["EXTRACT", "TRIM", "SUBSTRING", "SUBSTR", "OVERLAY"];

/// The first thing in `sql` that an agent may not run on DuckDB, as a short name for the refusal
/// (a function, a statement, `FROM '…'`), or `None`.
pub fn blocked(sql: &str, options: &SqlLexOptions) -> Option<String> {
    let scan = tokens(sql, options);
    scan.tokens
        .split(|token| token.kind == SqlTokenKind::Semicolon)
        .find_map(|statement| {
            let words = Statement { sql, tokens: statement };
            words
                .verb()
                .or_else(|| words.function())
                .or_else(|| words.file_as_table())
        })
}

struct Statement<'a> {
    sql: &'a str,
    tokens: &'a [SqlToken],
}

impl Statement<'_> {
    fn text(&self, at: usize) -> &str {
        &self.sql[self.tokens[at].range.clone()]
    }

    fn kind(&self, at: usize) -> Option<SqlTokenKind> {
        self.tokens.get(at).map(|token| token.kind)
    }

    /// The token at `at` is this bare word.
    fn is(&self, at: usize, word: &str) -> bool {
        self.kind(at) == Some(SqlTokenKind::Word) && self.text(at).eq_ignore_ascii_case(word)
    }

    fn is_one_of(&self, at: usize, words: &[&str]) -> bool {
        words.iter().any(|word| self.is(at, word))
    }

    fn is_symbol(&self, at: usize, symbol: u8) -> bool {
        self.kind(at) == Some(SqlTokenKind::Other) && self.sql.as_bytes()[self.tokens[at].range.start] == symbol
    }

    /// The index of the statement's first word: after opening parentheses and `EXPLAIN [ANALYZE
    /// | VERBOSE] [(options)]`, which run the statement that follows.
    fn first_word(&self) -> usize {
        self.statement_start(0)
    }

    /// [`first_word`](Self::first_word) of a statement that starts at token `from`.
    fn statement_start(&self, from: usize) -> usize {
        let mut at = from;
        loop {
            while self.kind(at) == Some(SqlTokenKind::Open) {
                at += 1;
            }
            if !self.is(at, "EXPLAIN") {
                return at;
            }
            at += 1;
            while self.is_one_of(at, &["ANALYZE", "ANALYSE", "VERBOSE"]) {
                at += 1;
            }
            if self.kind(at) == Some(SqlTokenKind::Open) {
                let mut depth = 0usize;
                while let Some(kind) = self.kind(at) {
                    at += 1;
                    match kind {
                        SqlTokenKind::Open => depth += 1,
                        SqlTokenKind::Close => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    /// `COPY`, `ATTACH`, `INSTALL`, ..., `CREATE | DROP | ALTER [OR REPLACE] [TEMPORARY |
    /// PERSISTENT] SECRET | EXTENSION | EXTERNAL`, `UPDATE EXTENSIONS`, a setting that writes log or
    /// profile files, at the start of the statement and of a parenthesis that holds a statement.
    fn verb(&self) -> Option<String> {
        let first = self.first_word();
        self.verb_at(first)
            .or_else(|| self.nested_verb())
            .or_else(|| self.output_setting(first))
    }

    /// A statement in parentheses that is not the statement's start: the body of a CTE
    /// (`WITH x AS (COPY t TO 'f') SELECT 1`, a `CTEDMLBody` is any statement), or `((ATTACH 'f'))`.
    fn nested_verb(&self) -> Option<String> {
        (0..self.tokens.len())
            .filter(|&at| {
                self.kind(at) == Some(SqlTokenKind::Open)
                    && (at == 0
                        || self.kind(at - 1) == Some(SqlTokenKind::Open)
                        || self.is_one_of(at - 1, &["AS", "MATERIALIZED"]))
            })
            .find_map(|at| self.verb_at(self.statement_start(at + 1)))
    }

    /// `PRAGMA enable_profiling`, `SET profiling_output = ...`, `CALL enable_logging(...)`: not
    /// reached as a function when written as a statement.
    fn output_setting(&self, first: usize) -> Option<String> {
        if !self.is_one_of(first, &["PRAGMA", "SET", "RESET", "CALL"]) {
            return None;
        }
        (first + 1..self.tokens.len()).find_map(|at| {
            (self.kind(at) == Some(SqlTokenKind::Word)
                && OUTPUT_SETTINGS
                    .iter()
                    .any(|pattern| crate::functions::glob_matches(pattern, self.text(at))))
            .then(|| {
                format!(
                    "{} {}",
                    self.text(first).to_ascii_uppercase(),
                    clean_name(self.text(at), MAX_NAME_CHARS)
                )
            })
        })
    }

    /// The verb check for a statement that starts at token `first`.
    fn verb_at(&self, first: usize) -> Option<String> {
        if self.kind(first) != Some(SqlTokenKind::Word) {
            return None;
        }
        if let Some(verb) = VERBS.iter().find(|verb| self.is(first, verb)) {
            return Some((*verb).to_owned());
        }
        if self.is(first, "FORCE") && self.is(first + 1, "INSTALL") {
            return Some("FORCE INSTALL".to_owned());
        }
        if self.is(first, "UPDATE") && self.is(first + 1, "EXTENSIONS") {
            return Some("UPDATE EXTENSIONS".to_owned());
        }
        if self.is_one_of(first, &["CREATE", "DROP", "ALTER"]) {
            let mut at = first + 1;
            if self.is(at, "OR") && self.is(at + 1, "REPLACE") {
                at += 2;
            }
            if self.is_one_of(at, &["TEMPORARY", "TEMP", "PERSISTENT"]) {
                at += 1;
            }
            if let Some(kind) = CREATE_KINDS.iter().find(|kind| self.is(at, kind)) {
                return Some(format!("{} {kind}", self.text(first).to_ascii_uppercase()));
            }
        }
        None
    }

    /// A call of a function of [`FUNCTIONS`], in any position, as written (`main."read_csv"(`).
    fn function(&self) -> Option<String> {
        (0..self.tokens.len()).find_map(|at| {
            if self.kind(at + 1) != Some(SqlTokenKind::Open) {
                return None;
            }
            let name = match self.kind(at)? {
                SqlTokenKind::Word => self.text(at).to_owned(),
                SqlTokenKind::Quoted => unquote_name(self.text(at))?,
                _ => return None,
            };
            // `INSERT INTO query (a, b)`: a table that is declared, not called.
            if at > 0 && self.is(at - 1, "INTO") {
                return None;
            }
            FUNCTIONS
                .iter()
                .any(|pattern| crate::functions::glob_matches(pattern, &name))
                .then(|| clean_name(&name, MAX_NAME_CHARS))
        })
    }

    /// A string, or a quoted name that looks like a path, where a table is expected:
    /// `FROM '/x/y.csv'`, `JOIN 'https://h/x.parquet'`, `FROM t, 'x.csv'`, `DESCRIBE "a/b.csv"`.
    /// DuckDB reads such a name as a file (a replacement scan); no function is called.
    fn file_as_table(&self) -> Option<String> {
        struct Frame {
            /// The parentheses of `EXTRACT(.. FROM ..)` and the like.
            expression: bool,
            /// A `FROM` list is open at this depth.
            in_from: bool,
        }
        let delete = self.is_one_of(self.first_word(), &["DELETE", "MERGE"]);
        let mut frames = vec![Frame { expression: false, in_from: false }];
        let mut expecting_table = false;
        let mut at = 0;
        while at < self.tokens.len() {
            let top = frames.last_mut()?;
            match self.tokens[at].kind {
                SqlTokenKind::Open => {
                    let expression = at > 0 && self.is_one_of(at - 1, FROM_IN_EXPRESSION);
                    frames.push(Frame { expression, in_from: false });
                    // `FROM (SELECT ..)` is not a name; `FROM ('x.csv')` is not valid SQL either.
                    at += 1;
                    continue;
                }
                SqlTokenKind::Close => {
                    if frames.len() > 1 {
                        frames.pop();
                    }
                }
                SqlTokenKind::Comma if top.in_from => expecting_table = true,
                SqlTokenKind::Word => {
                    let table_word = self.is_one_of(at, TABLE_WORDS);
                    // `a IS DISTINCT FROM 'x'` compares, and `EXTRACT(.. FROM 'x')` reads a value.
                    let value_follows = (at > 0 && self.is(at - 1, "DISTINCT")) || (top.expression && self.is(at, "FROM"));
                    if table_word && !value_follows {
                        expecting_table = true;
                        if self.is_one_of(at, &["FROM", "JOIN"]) || (delete && self.is(at, "USING")) {
                            top.in_from = true;
                        }
                    } else if ENDS_FROM_LIST.iter().any(|word| self.is(at, word)) {
                        top.in_from = false;
                        expecting_table = false;
                    } else if expecting_table
                        && self.is_one_of(at, &["U", "u"])
                        && self.is_symbol(at + 1, b'&')
                    {
                        // `U&'..'`: a string with Unicode escapes, still the string.
                        at += 2;
                        continue;
                    } else if expecting_table && self.is(at, "BY") && self.is(at.saturating_sub(1), "JOIN") {
                        // `JOIN BY (TYPE inner) '..'`: the options are not the table.
                        at = self.after_group(at + 1);
                        continue;
                    } else if expecting_table && self.prefix_alias(at) {
                        // `FROM a: '..'`: the alias comes first.
                        at += 2;
                        continue;
                    } else if expecting_table && self.dotted_file(at) {
                        return Some("FROM data.csv (a file used as a table)".to_owned());
                    } else {
                        expecting_table = false;
                    }
                }
                SqlTokenKind::Quoted if expecting_table && self.prefix_alias(at) => {
                    at += 2;
                    continue;
                }
                SqlTokenKind::Quoted if expecting_table && self.dotted_file(at) => {
                    return Some("FROM \"a\".\"csv\" (a file used as a table)".to_owned());
                }
                SqlTokenKind::Quoted if expecting_table => {
                    // `'..'`, `$$..$$` and `E'..'` are strings; `".."` and `` `..` `` are names.
                    let is_file = match unquote_name(self.text(at)) {
                        Some(name) => looks_like_path(&name),
                        None => true,
                    };
                    if is_file {
                        return Some("FROM '…' (a file or URL used as a table)".to_owned());
                    }
                    expecting_table = false;
                }
                _ => expecting_table = false,
            }
            at += 1;
        }
        None
    }
}

impl Statement<'_> {
    /// The index after the parenthesised group that starts at `at` (`at` itself when none does).
    fn after_group(&self, at: usize) -> usize {
        if self.kind(at) != Some(SqlTokenKind::Open) {
            return at;
        }
        let mut depth = 0usize;
        let mut index = at;
        while let Some(kind) = self.kind(index) {
            index += 1;
            match kind {
                SqlTokenKind::Open => depth += 1,
                SqlTokenKind::Close => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
        }
        index
    }

    /// The name at `at` is followed by a lone `:` (`a: 'x.csv'`, DuckDB's prefix alias).
    fn prefix_alias(&self, at: usize) -> bool {
        let name = match self.kind(at) {
            Some(SqlTokenKind::Word) => true,
            Some(SqlTokenKind::Quoted) => unquote_name(self.text(at)).is_some(),
            _ => false,
        };
        name && self.is_symbol(at + 1, b':') && !self.is_symbol(at + 2, b':')
    }

    /// A dotted name that ends in a data file's extension (`data.csv`, `"a"."csv"`), at `at`.
    fn dotted_file(&self, at: usize) -> bool {
        let part = |index: usize| -> Option<String> {
            match self.kind(index)? {
                SqlTokenKind::Word => Some(self.text(index).to_owned()),
                SqlTokenKind::Quoted => unquote_name(self.text(index)),
                _ => None,
            }
        };
        if part(at).is_none() {
            return false;
        }
        let mut last = at;
        while self.is_symbol(last + 1, b'.') && part(last + 2).is_some() {
            last += 2;
        }
        last > at
            && part(last).is_some_and(|name| {
                DATA_EXTENSIONS.iter().any(|extension| name.eq_ignore_ascii_case(extension))
            })
    }
}

/// The name inside a `"…"` or `` `…` `` token (a doubled quote is one quote); `None` for a string.
/// DuckDB reads both as names, so both can hide a function (`` `read_text`('/x') ``) or a table.
fn unquote_name(token: &str) -> Option<String> {
    let quote = match token.as_bytes().first()? {
        b'"' => '"',
        b'`' => '`',
        _ => return None,
    };
    let inner = &token[1..];
    let inner = inner.strip_suffix(quote).unwrap_or(inner);
    Some(inner.replace(&format!("{quote}{quote}"), &quote.to_string()))
}

/// A quoted name DuckDB would not find as a table but would try as a file: it has a path
/// separator, a drive or URL colon, a home mark or an extension.
fn looks_like_path(name: &str) -> bool {
    name.contains(['/', '\\', ':', '~', '.'])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(sql: &str) -> Option<String> {
        blocked(sql, &SqlLexOptions::duckdb())
    }

    #[test]
    fn table_functions_that_touch_files_are_blocked_everywhere() {
        for name in [
            "read_csv", "read_csv_auto", "read_parquet", "read_json", "read_json_auto", "read_ndjson",
            "read_text", "read_blob", "parquet_scan", "glob", "sniff_csv", "parquet_metadata",
            "parquet_schema", "delta_scan", "iceberg_scan", "query_table", "query", "sqlite_scan",
            "postgres_scan", "read_xlsx", "st_read", "getenv", "duckdb_secrets", "which_secret",
        ] {
            for sql in [
                format!("SELECT * FROM {name}('/etc/passwd')"),
                format!("select * from {}('/etc/passwd')", name.to_uppercase()),
                format!("SELECT * FROM {name} ('/etc/passwd')"),
                format!("SELECT * FROM main.{name}('/etc/passwd')"),
                format!("SELECT * FROM \"{name}\"('/etc/passwd')"),
                format!("SELECT * FROM `{name}`('/etc/passwd')"),
                format!("SELECT * FROM main.`{name}`('/etc/passwd')"),
                // DuckDB reads a vertical tab as white space.
                format!("SELECT * FROM {name}\x0b('/etc/passwd')"),
                format!("SELECT * FROM {name}\x0b\x0b/* x */\x0b('/etc/passwd')"),
                format!("SELECT * FROM {name}/* hidden */('/etc/passwd')"),
                format!("SELECT x FROM (SELECT * FROM {name}('/etc/passwd')) t"),
                format!("WITH t AS (SELECT * FROM {name}('/etc/passwd')) SELECT * FROM t"),
                format!("SELECT (SELECT count(*) FROM {name}('/etc/passwd'))"),
                format!("SELECT 1 WHERE EXISTS (SELECT 1 FROM {name}('/etc/passwd'))"),
                format!("CREATE TABLE t AS SELECT * FROM {name}('/etc/passwd')"),
                format!("CREATE MACRO m(f) AS TABLE SELECT * FROM {name}(f)"),
                format!("CREATE MACRO m(f) AS {name}(f)"),
                format!("CALL {name}('/etc/passwd')"),
                format!("EXPLAIN SELECT * FROM {name}('/etc/passwd')"),
                format!("SELECT 1; SELECT * FROM {name}('/etc/passwd')"),
            ] {
                assert_eq!(hit(&sql).map(|found| found.to_ascii_lowercase()).as_deref(), Some(name), "{sql}");
            }
        }
    }

    #[test]
    fn a_file_or_url_in_the_place_of_a_table_is_blocked() {
        for sql in [
            "FROM '/Users/x/.ssh/id_rsa'",
            "SELECT * FROM '/Users/x/.ssh/id_rsa'",
            "SELECT * FROM 'https://evil/x.csv'",
            "SELECT * FROM 's3://bucket/key.parquet'",
            "SELECT * FROM 'relative.csv'",
            "select * from\n'x.csv'",
            "SELECT * FROM E'x\\x2ecsv'",
            "SELECT * FROM U&'x.csv'",
            "SELECT * FROM $$/etc/passwd$$",
            "SELECT * FROM $t$/etc/passwd$t$",
            "SELECT * FROM \"/etc/passwd\"",
            "SELECT * FROM \"x.csv\"",
            "SELECT * FROM \"https://evil/x.csv\"",
            "SELECT * FROM 'a.csv' AS t",
            "SELECT * FROM t JOIN 'a.csv' ON t.id = 1",
            "SELECT * FROM t JOIN 'a.csv' USING (id)",
            "SELECT * FROM t, 'a.csv'",
            "SELECT * FROM t AS a, u AS b, 'a.csv'",
            "SELECT * FROM generate_series(1, 3), 'a.csv'",
            "SELECT * FROM (SELECT 1) AS a, \"/etc/passwd\"",
            "SELECT * FROM (SELECT * FROM 'a.csv')",
            "SELECT * FROM (SELECT 1) WHERE x IN (SELECT y FROM 'a.csv')",
            "WITH t AS (SELECT * FROM 'a.csv') SELECT * FROM t",
            "SELECT * FROM t UNION ALL SELECT * FROM 'a.csv'",
            "SELECT (SELECT count(*) FROM 'a.csv')",
            "SELECT 1 /* x */ FROM /* y */ 'a.csv'",
            "SELECT 1 FROM -- y\n'a.csv'",
            "INSERT INTO t SELECT * FROM 'a.csv'",
            "CREATE TABLE t AS FROM 'a.csv'",
            "CREATE TABLE t AS SELECT * FROM 'a.csv'",
            "UPDATE t SET a = b.a FROM 'a.csv' AS b",
            "DELETE FROM t USING 'a.csv' AS b",
            "DESCRIBE 'a.csv'",
            "SUMMARIZE 'a.csv'",
            "PIVOT 'a.csv' ON x USING sum(y)",
            "TABLE 'a.csv'",
            "EXPLAIN SELECT * FROM 'a.csv'",
            "SELECT 1; SELECT * FROM 'a.csv'",
        ] {
            assert_eq!(hit(sql).as_deref(), Some("FROM '…' (a file or URL used as a table)"), "{sql}");
        }
    }

    #[test]
    fn statements_that_change_the_environment_or_move_files_are_blocked() {
        for (sql, name) in [
            ("COPY t TO '/tmp/x.csv'", "COPY"),
            ("COPY t FROM '/tmp/x.csv'", "COPY"),
            ("copy (SELECT 1) to '/tmp/x.parquet' (FORMAT parquet)", "COPY"),
            ("/* a */ COPY t TO '/tmp/x.csv'", "COPY"),
            ("(COPY t TO '/tmp/x.csv')", "COPY"),
            ("EXPLAIN ANALYZE COPY t TO '/tmp/x.csv'", "COPY"),
            ("EXPLAIN (ANALYZE) COPY t TO '/tmp/x.csv'", "COPY"),
            ("SELECT 1; COPY t TO '/tmp/x.csv'", "COPY"),
            ("EXPORT DATABASE '/tmp/out'", "EXPORT"),
            ("IMPORT DATABASE '/tmp/out'", "IMPORT"),
            ("ATTACH '/other.db'", "ATTACH"),
            ("attach database '/other.db' AS o (READ_ONLY)", "ATTACH"),
            ("ATTACH 'host=h dbname=d' AS p (TYPE postgres)", "ATTACH"),
            ("DETACH o", "DETACH"),
            ("INSTALL httpfs", "INSTALL"),
            ("install httpfs FROM community", "INSTALL"),
            ("FORCE INSTALL httpfs", "FORCE INSTALL"),
            ("LOAD httpfs", "LOAD"),
            ("UPDATE EXTENSIONS", "UPDATE EXTENSIONS"),
            ("CREATE SECRET s (TYPE s3, KEY_ID 'a', SECRET 'b')", "CREATE SECRET"),
            ("CREATE OR REPLACE PERSISTENT SECRET s (TYPE s3)", "CREATE SECRET"),
            ("CREATE TEMPORARY SECRET s (TYPE s3)", "CREATE SECRET"),
            ("DROP SECRET s", "DROP SECRET"),
            ("DROP PERSISTENT SECRET s", "DROP SECRET"),
            ("CONNECT 'target' (TYPE postgres)", "CONNECT"),
            ("DISCONNECT", "DISCONNECT"),
            ("CREATE EXTENSION REPOSITORY r URL 'https://evil'", "CREATE EXTENSION"),
            ("CREATE OR REPLACE EXTENSION REPOSITORY r URL 'https://evil'", "CREATE EXTENSION"),
            ("CREATE EXTERNAL RESOURCE r (TYPE http)", "CREATE EXTERNAL"),
            ("CREATE OR REPLACE EXTERNAL RESOURCE r (TYPE http)", "CREATE EXTERNAL"),
            ("REGISTER EXTERNAL RESOURCE r", "REGISTER"),
            ("DESTROY EXTERNAL RESOURCE r", "DESTROY"),
            ("ATTACH TO EXTERNAL RESOURCE r", "ATTACH"),
            ("CONNECT TO EXTERNAL RESOURCE r", "CONNECT"),
            // A CTE body is any statement; so is a parenthesis at the start.
            ("WITH x AS (COPY t TO '/tmp/f') SELECT 1", "COPY"),
            ("WITH x AS (COPY (SELECT 1) TO '/tmp/f') SELECT 1", "COPY"),
            ("WITH a AS (SELECT 1), x AS MATERIALIZED (ATTACH '/f' AS o) SELECT 1", "ATTACH"),
            ("WITH x AS NOT MATERIALIZED (INSTALL httpfs) SELECT 1", "INSTALL"),
            ("WITH RECURSIVE x AS (LOAD httpfs) SELECT 1", "LOAD"),
            ("WITH x AS (EXPLAIN ANALYZE COPY t TO '/tmp/f') SELECT 1", "COPY"),
            ("WITH x AS ((ATTACH '/f')) SELECT 1", "ATTACH"),
            ("((ATTACH '/f'))", "ATTACH"),
            ("PRAGMA enable_profiling = 'json'", "PRAGMA enable_profiling"),
            ("PRAGMA profiling_output = '/tmp/x'", "PRAGMA profiling_output"),
            ("SET profiling_output = '/tmp/x'", "SET profiling_output"),
            ("SET logging_storage = 'file'", "SET logging_storage"),
            ("CALL enable_logging(storage = 'file', storage_path = '/tmp/x')", "CALL enable_logging"),
            ("PRAGMA log_query_path = '/tmp/x'", "PRAGMA log_query_path"),
        ] {
            assert_eq!(hit(sql).as_deref(), Some(name), "{sql}");
        }
    }

    #[test]
    fn sql_carried_in_a_string_and_files_written_by_logging_are_blocked() {
        for (sql, name) in [
            ("SELECT json_execute_serialized_sql(json_serialize_sql('SELECT * FROM read_text(''~/.ssh/id_rsa'')'))", "json_execute_serialized_sql"),
            ("SELECT * FROM json_execute_serialized_sql('{}')", "json_execute_serialized_sql"),
            ("SELECT json_serialize_sql('SELECT 1')", "json_serialize_sql"),
            ("SELECT json_deserialize_sql('{}')", "json_deserialize_sql"),
            ("SELECT * FROM enable_logging(storage = 'file', storage_path = '/tmp/x')", "enable_logging"),
            ("SELECT disable_logging()", "disable_logging"),
            ("SELECT enable_profiling(format = 'json', save_location = '/tmp/x')", "enable_profiling"),
            ("SELECT 1; SELECT disable_profiling()", "disable_profiling"),
        ] {
            assert_eq!(hit(sql).as_deref(), Some(name), "{sql}");
        }
    }

    #[test]
    fn grammar_positions_where_a_string_is_a_file_are_blocked() {
        for sql in [
            // A prefix alias comes before the table: `alias: 'file'`.
            "SELECT * FROM a: '~/x.json'",
            "FROM a: '~/x.json'",
            "SELECT * FROM t, a: '~/x.json'",
            "SELECT * FROM \"a\": '~/x.json'",
            "SELECT * FROM t JOIN b: 'a.csv' ON true",
            "SELECT * FROM a: b: '~/x.json'",
            "SHOW '~/file.json'",
            "PIVOT_WIDER '~/x.json' ON a USING sum(b)",
            "PIVOT_LONGER '~/x.json' ON a INTO NAME n VALUE v",
            "SELECT * FROM t JOIN BY (TYPE inner) '~/x.json'",
            "SELECT * FROM t JOIN BY (TYPE inner, a = (1)) 'x.csv' ON true",
            "MERGE INTO t USING '~/x.json' ON t.id = 1 WHEN MATCHED THEN DELETE",
            "MERGE INTO t USING 'x.csv' AS s ON t.id = s.id WHEN NOT MATCHED THEN INSERT VALUES (s.id)",
            "MERGE INTO t USING \"x.csv\" ON true WHEN MATCHED THEN DELETE",
            // An unquoted or part-quoted dotted name is a replacement scan of the file with that name.
            "SELECT * FROM data.csv",
            "SELECT * FROM DATA.CSV",
            "SELECT * FROM \"a\".\"csv\"",
            "SELECT * FROM a.\"json\"",
            "SELECT * FROM main.data.parquet",
            "SELECT * FROM t JOIN data.ndjson ON true",
            "SELECT * FROM t, logs.jsonl",
            "SELECT * FROM a: data.tsv",
            "DESCRIBE data.csv",
        ] {
            assert!(hit(sql).is_some(), "{sql}");
        }
    }

    #[test]
    fn a_lexer_that_misses_backtick_names_cannot_hide_a_file_read() {
        // PostgreSQL's lexer would read the `'` as a string and see no statement boundary at all.
        for sql in [
            "SELECT 1 AS `'`; SELECT * FROM read_text('/etc/passwd'); SELECT 1 AS `'`",
            "SELECT 1 AS `'`; COPY t TO '/tmp/x'; SELECT 1 AS `'`",
            "SELECT 1 AS `'`; SELECT * FROM '/etc/passwd'; SELECT 1 AS `'`",
            "SELECT 1 AS a`'`; SELECT * FROM read_text('/etc/passwd'); SELECT 1 AS `'`",
        ] {
            assert!(hit(sql).is_some(), "{sql}");
        }
    }

    #[test]
    fn a_backtick_name_is_a_name_like_a_double_quoted_one() {
        for sql in [
            "SELECT * FROM `data.csv`",
            "SELECT * FROM `/etc/passwd`",
            "SELECT * FROM t, `x.csv`",
            "SELECT * FROM `a`.`csv`",
            "SELECT * FROM a.`json`",
            "SELECT * FROM `a`: '~/x.json'",
            "SELECT * FROM `a`: `x.csv`",
        ] {
            assert!(hit(sql).is_some(), "{sql}");
        }
        // A doubled backtick is one backtick, as a doubled `"` is one `"`.
        assert!(hit("SELECT * FROM `my``read_text`('/etc/passwd')").is_none());
        assert!(hit("SELECT * FROM `read``_text`('/etc/passwd')").is_none());
        // Backtick names of tables that are no file are plain tables.
        for sql in [
            "SELECT * FROM `orders`",
            "SELECT * FROM `my table`",
            "SELECT * FROM main.`Order Items` AS `o`",
            "SELECT * FROM t JOIN `orders` ON true, `lines`",
            "SELECT `a`, `b` FROM `orders` WHERE `a` = 1",
            "INSERT INTO `query` (a, b) VALUES (1, 2)",
            "SELECT 1 AS `it's`",
        ] {
            assert_eq!(hit(sql), None, "{sql}");
        }
    }

    #[test]
    fn what_only_looks_like_one_runs() {
        for sql in [
            "SELECT sum(load), copy, attach(1), import FROM t",
            "SELECT coalesce(connect, 0), register FROM t WHERE destroy = 1",
            "SELECT * FROM t WHERE a IN (SELECT load FROM u)",
            "SELECT * FROM t WHERE x = 'csv' AND y = 'a.csv'",
            "SELECT * FROM main.json_docs, t.json_col",
            "SELECT t.json, u.csv FROM t JOIN u ON t.k = u.k",
            "SELECT * FROM t JOIN u USING (id) WHERE t.x = 'a.csv'",
            "SELECT * FROM t AS a JOIN u AS b USING (k)",
            "SELECT * FROM t WHERE k IN (SELECT k FROM u WHERE v = 'x.json')",
            "SHOW TABLES",
            "SHOW ALL TABLES",
            "SET schema = 'main'",
            "SET threads = 1",
            "PRAGMA table_info('t')",
            "SELECT 'a: ''x.csv''' AS note",
            "SELECT a: 1 FROM t",
            "SELECT * FROM t WHERE logging_level = 1 AND profiling = 2",
            "MERGE INTO t USING u ON t.id = u.id WHEN MATCHED THEN DELETE",
            "WITH x AS (SELECT 1) SELECT * FROM x",
            "WITH x AS MATERIALIZED (SELECT * FROM t) SELECT * FROM x",
            "SELECT 'read_csv(''x'')' AS note",
            "SELECT 'FROM ''/etc/passwd''' AS note",
            "-- read_csv('/etc/passwd')\nSELECT 1",
            "/* COPY t TO 'x' */ SELECT 1",
            "SELECT $$ FROM '/etc/passwd' $$",
            "SELECT * FROM t WHERE a = 'x.csv' AND b IN ('/a', '/b')",
            "SELECT 'a', 'b/c' FROM t",
            "SELECT a, 'x' FROM t GROUP BY a, 'x'",
            "SELECT * FROM t ORDER BY a, 'x'",
            "SELECT * FROM t WHERE a IS DISTINCT FROM 'x/y'",
            "SELECT extract(year FROM '2024-01-01'::DATE), trim(BOTH 'x' FROM 'xax'), substring('a/b' FROM 1)",
            "SELECT * FROM t, u WHERE t.a = 'x.csv'",
            "SELECT * FROM t AS \"a.b\"",
            "SELECT * FROM \"my table\"",
            "SELECT * FROM main.\"Order Items\"",
            "SELECT * FROM t AS a JOIN u ON a.k = u.k WHERE u.p = '/x'",
            "INSERT INTO t VALUES ('a/b'), ('c.csv')",
            "INSERT INTO query (a, b) VALUES (1, 2)",
            "UPDATE t SET a = 'x.csv', b = '/y'",
            "SELECT copy, load, export, attach FROM t",
            "SELECT read_count, scan_total, glob_x FROM t",
            "SELECT * FROM t WHERE name LIKE '%.csv'",
            "CREATE TABLE secret (id INT)",
            "SELECT * FROM secret",
            "CREATE TABLE t (a INT)",
            "SELECT * FROM duckdb_tables()",
            "SELECT * FROM generate_series(1, 3)",
            "DESCRIBE t",
            "SUMMARIZE t",
            "EXPLAIN SELECT 1",
            "SELECT 1",
        ] {
            assert_eq!(hit(sql), None, "{sql}");
        }
    }
}
