//! Functions and clauses an agent may never run, whatever its access level.
//!
//! The statement classifier proves a statement is a *pure read lexically*,
//! but a pure `SELECT` can still reach outside the table data: read server
//! files, call out to the network, run server-side code, change server state
//! (`nextval`, advisory locks) or burn CPU. This module scans the lexed token
//! stream (never strings, comments or dollar-quoted text) for calls of such
//! functions (identifier followed by `(`, also schema-qualified, quoted,
//! any letter case) and for a few clauses, per dialect.
//!
//! **A denylist is never complete.** It is the first of two layers; the second
//! is the driver executing the statement read-only
//! (`QueryRequest.payload.read_only`, capability `query.read_only`), which
//! stops a write whatever hides it. Reads of files and the network are only
//! stopped here (and by the database user's own privileges).

use crate::ports::ConnectionKind;
use engine_driver_kit::sql::SqlLexOptions;

/// A pattern for function names; `*` matches any run of characters.
type Pattern = &'static str;

/// ClickHouse table functions and other functions that reach beyond the
/// server's tables: object storage and the network (`s3*`, `gcs*`, `hdfs*`,
/// `azureBlobStorage*`, `iceberg*`, `deltaLake*`, `hudi*`, `hive`, `url`,
/// `urlCluster`), server files (`file`, `fileCluster`), other servers
/// (`remote*`, `cluster*`, `mysql`, `postgresql`, `mongodb`, `redis`,
/// `sqlite`, `odbc`, `jdbc`, `ytsaurus`), message brokers (`kafka`, `nats`,
/// `rabbitmq`), a program (`executable`), the request body (`input`),
/// dictionaries, which read external sources (`dictGet*`) and MergeTree
/// internals (`mergeTreeIndex`).
///
/// `file` and `url` are listed by exact name, not `file*` / `url*`: those
/// globs would also refuse harmless built-ins (`filesystemAvailable`,
/// `filesystemCapacity`, `URLHash`, `URLHierarchy`, `URLPathHierarchy`).
/// Every other glob below was checked against ClickHouse's function list and
/// catches only functions of its family.
const CLICKHOUSE: &[Pattern] = &[
    "url",
    "urlCluster",
    "file",
    "fileCluster",
    "remote*",
    "cluster*",
    "s3*",
    "gcs*",
    "azureBlobStorage*",
    "hdfs*",
    "iceberg*",
    "deltaLake*",
    "hudi*",
    "hive",
    "mysql",
    "postgresql",
    "mongodb",
    "redis",
    "sqlite",
    "odbc",
    "jdbc",
    "ytsaurus",
    "kafka",
    "nats",
    "rabbitmq",
    "executable",
    "input",
    "dictGet*",
    "mergeTreeIndex",
];

/// ClickHouse: a `SETTINGS` clause (`SELECT ... SETTINGS max_threads = 1`)
/// changes how the server runs the query (limits, `readonly`-adjacent
/// switches, external access); agents may not send one. Extra defence: the
/// settings the driver itself sends are not affected.
const CLICKHOUSE_CLAUSE: &str = "SETTINGS";

/// PostgreSQL: `pg_read_*`, `pg_ls_*`, `pg_stat_file`, `lo_import` /
/// `lo_export` read or write server files, `pg_file_*` / `pg_logdir_ls`
/// (adminpack) write, rename and remove them, `lo_from_bytea` / `lo_put` /
/// `lo_create` / `lo_creat` / `lo_unlink` change large objects; `pg_promote`,
/// `pg_switch_wal`, `pg_stat_reset*`, `pg_current_logfile` change or probe
/// server state; `dblink*` opens connections and runs remote SQL;
/// `query_to_xml*` / `cursor_to_xml` run the SQL text they are given;
/// `pg_terminate_backend`, `pg_cancel_backend`, `pg_reload_conf`,
/// `pg_rotate_logfile`, `set_config` change server or session state;
/// `nextval` / `setval` change sequences; advisory locks hold server-wide
/// locks; `pg_logical_emit_message`, `pg_notify` write to WAL / listeners;
/// `pg_create_*`, `pg_drop_*`, `pg_replication_*` manage slots, origins and
/// replication.
const POSTGRESQL: &[Pattern] = &[
    "pg_read_file",
    "pg_read_binary_file",
    "pg_ls_dir",
    "pg_ls_logdir",
    "pg_ls_waldir",
    "pg_ls_*",
    "pg_stat_file",
    "lo_import",
    "lo_export",
    "dblink*",
    "query_to_xml",
    "query_to_xml_and_xmlschema",
    "cursor_to_xml",
    "pg_terminate_backend",
    "pg_cancel_backend",
    "pg_reload_conf",
    "pg_rotate_logfile",
    "set_config",
    "nextval",
    "setval",
    "pg_advisory_*lock*",
    "pg_try_advisory_*",
    "pg_logical_emit_message",
    "pg_create_*",
    "pg_drop_*",
    "pg_replication_*",
    "pg_notify",
    "pg_file_*",
    "pg_logdir_ls",
    "pg_ls_tmpdir",
    "pg_ls_archive_statusdir",
    "pg_promote",
    "pg_switch_wal",
    "pg_stat_reset*",
    "pg_current_logfile",
    "lo_from_bytea",
    "lo_put",
    "lo_unlink",
    "lo_create",
    "lo_creat",
];

/// MySQL / MariaDB: `load_file` reads server files; `sys_exec` / `sys_eval`
/// (UDFs) run programs; `get_lock` / `release_lock` hold server-wide named
/// locks; `benchmark` burns CPU (`sleep` is allowed: the statement timeout
/// applies).
const MYSQL: &[Pattern] = &[
    "load_file",
    "sys_exec",
    "sys_eval",
    "get_lock",
    "release_lock",
    "benchmark",
];

/// SQLite: `load_extension` loads native code; `readfile` / `writefile` /
/// `edit` (CLI extensions) touch files; `fts3_tokenizer` registers a
/// tokenizer by pointer.
const SQLITE: &[Pattern] = &[
    "load_extension",
    "readfile",
    "writefile",
    "edit",
    "fts3_tokenizer",
];

fn table(kind: ConnectionKind) -> &'static [Pattern] {
    match kind {
        ConnectionKind::ClickHouse => CLICKHOUSE,
        ConnectionKind::PostgreSql => POSTGRESQL,
        ConnectionKind::MySql | ConnectionKind::MariaDb => MYSQL,
        ConnectionKind::Sqlite => SQLITE,
        ConnectionKind::MongoDb | ConnectionKind::Redis => &[],
    }
}

fn glob_matches(pattern: &str, name: &str) -> bool {
    let pattern = pattern.to_ascii_lowercase();
    let name = name.to_ascii_lowercase();
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or_default();
    if !name.starts_with(first) {
        return false;
    }
    let mut rest = &name[first.len()..];
    let parts: Vec<&str> = parts.collect();
    let Some((last, middle)) = parts.split_last() else {
        return rest.is_empty();
    };
    for part in middle {
        match rest.find(part) {
            Some(at) => rest = &rest[at + part.len()..],
            None => return false,
        }
    }
    rest.ends_with(last)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Word(String),
    /// A quoted identifier (`"x"`, `` `x` ``, `[x]`), `unicode` for PostgreSQL `U&"..."`.
    Ident {
        text: String,
        unicode: bool,
    },
    Open,
    Dot,
    Semicolon,
    Equals,
    Other,
}

struct Scanner<'a> {
    bytes: &'a [u8],
    text: &'a str,
    options: &'a SqlLexOptions,
    at: usize,
    /// Open `/*! ... */` executable comments (their content is code).
    executable_depth: u32,
}

impl<'a> Scanner<'a> {
    fn new(text: &'a str, options: &'a SqlLexOptions) -> Self {
        Self {
            bytes: text.as_bytes(),
            text,
            options,
            at: 0,
            executable_depth: 0,
        }
    }

    fn peek(&self, offset: usize) -> Option<u8> {
        self.bytes.get(self.at + offset).copied()
    }

    fn skip_quoted(&mut self, quote: u8, backslash: bool) -> String {
        let mut content = Vec::new();
        self.at += 1;
        while let Some(byte) = self.peek(0) {
            if backslash && byte == b'\\' {
                if let Some(next) = self.peek(1) {
                    content.push(byte);
                    content.push(next);
                }
                self.at += 2;
                continue;
            }
            if byte == quote {
                if self.peek(1) == Some(quote) {
                    content.push(quote);
                    self.at += 2;
                    continue;
                }
                self.at += 1;
                break;
            }
            content.push(byte);
            self.at += 1;
        }
        String::from_utf8_lossy(&content).into_owned()
    }

    fn skip_block_comment(&mut self) {
        let mut depth = 1;
        self.at += 2;
        while self.at < self.bytes.len() && depth > 0 {
            if self.options.nested_block_comments
                && self.peek(0) == Some(b'/')
                && self.peek(1) == Some(b'*')
            {
                depth += 1;
                self.at += 2;
            } else if self.peek(0) == Some(b'*') && self.peek(1) == Some(b'/') {
                depth -= 1;
                self.at += 2;
            } else {
                self.at += 1;
            }
        }
    }

    /// The rest of a line comment: PostgreSQL ends it at `\r` as well as `\n`, the other
    /// engines at `\n` only (the kit's `line_comment_ends_at_cr`).
    fn skip_line(&mut self) {
        while let Some(byte) = self.peek(0) {
            self.at += 1;
            if byte == b'\n' || (byte == b'\r' && self.options.line_comment_ends_at_cr) {
                break;
            }
        }
    }

    /// `$tag$` at the cursor, if the dialect has dollar quotes.
    fn dollar_tag(&self) -> Option<usize> {
        if !self.options.dollar_quotes || self.peek(0) != Some(b'$') {
            return None;
        }
        let mut end = self.at + 1;
        while end < self.bytes.len()
            && (self.bytes[end].is_ascii_alphanumeric() || self.bytes[end] == b'_')
        {
            end += 1;
        }
        let tag_is_numeric_start = self.bytes.get(self.at + 1).is_some_and(u8::is_ascii_digit);
        (self.bytes.get(end) == Some(&b'$') && !tag_is_numeric_start).then_some(end + 1)
    }

    fn next(&mut self, previous_e: bool) -> Option<Token> {
        loop {
            let byte = self.peek(0)?;
            match byte {
                b' ' | b'\t' | b'\r' | b'\n' | 0x0c => self.at += 1,
                b'-' if self.peek(1) == Some(b'-')
                    && (!self.options.dash_comment_needs_space
                        || self
                            .peek(2)
                            .is_none_or(|b| b.is_ascii_whitespace() || b.is_ascii_control())) =>
                {
                    self.skip_line();
                }
                b'#' if self.options.hash_comments => self.skip_line(),
                b'/' if self.peek(1) == Some(b'*') => {
                    let executable = self.options.executable_comments && self.peek(2) == Some(b'!')
                        || self.options.mariadb_comments
                            && self.peek(2) == Some(b'M')
                            && self.peek(3) == Some(b'!');
                    if executable {
                        // The content is code: skip the opener and any version digits.
                        self.at += if self.peek(2) == Some(b'M') { 4 } else { 3 };
                        while self.peek(0).is_some_and(|b| b.is_ascii_digit()) {
                            self.at += 1;
                        }
                        self.executable_depth += 1;
                    } else {
                        self.skip_block_comment();
                    }
                }
                b'*' if self.executable_depth > 0 && self.peek(1) == Some(b'/') => {
                    self.executable_depth -= 1;
                    self.at += 2;
                }
                _ => break,
            }
        }
        let byte = self.peek(0)?;
        let token = match byte {
            b'\'' => {
                let backslash =
                    self.options.backslash_escapes || (self.options.e_strings && previous_e);
                self.skip_quoted(b'\'', backslash);
                Token::Other
            }
            b'"' if self.options.double_quote_is_identifier => {
                let text = self.skip_quoted(b'"', self.options.backslash_in_identifiers);
                Token::Ident {
                    text,
                    unicode: false,
                }
            }
            b'"' => {
                self.skip_quoted(b'"', self.options.backslash_escapes);
                Token::Other
            }
            b'`' if self.options.backtick_identifiers => {
                let text = self.skip_quoted(b'`', self.options.backslash_in_identifiers);
                Token::Ident {
                    text,
                    unicode: false,
                }
            }
            b'[' if self.options.bracket_identifiers => {
                self.at += 1;
                let start = self.at;
                while self.at < self.bytes.len() && self.bytes[self.at] != b']' {
                    self.at += 1;
                }
                let text = self.text.get(start..self.at).unwrap_or_default().to_owned();
                self.at += 1;
                Token::Ident {
                    text,
                    unicode: false,
                }
            }
            b'$' if self.dollar_tag().is_some() => {
                let end = self.dollar_tag().unwrap_or(self.at + 1);
                let tag = self.text[self.at..end].to_owned();
                self.at = end;
                match self.text[self.at..].find(&tag) {
                    Some(close) => self.at += close + tag.len(),
                    None => self.at = self.bytes.len(),
                }
                Token::Other
            }
            b'(' => {
                self.at += 1;
                Token::Open
            }
            b'.' => {
                self.at += 1;
                Token::Dot
            }
            b';' => {
                self.at += 1;
                Token::Semicolon
            }
            byte if byte.is_ascii_alphabetic() || byte == b'_' || byte >= 0x80 => {
                let start = self.at;
                while self.at < self.bytes.len()
                    && (self.bytes[self.at].is_ascii_alphanumeric()
                        || matches!(self.bytes[self.at], b'_' | b'$')
                        || self.bytes[self.at] >= 0x80)
                {
                    self.at += 1;
                }
                // PostgreSQL `U&"..."` identifier with Unicode escapes.
                Token::Word(self.text.get(start..self.at).unwrap_or_default().to_owned())
            }
            byte if byte.is_ascii_digit() => {
                while self.at < self.bytes.len()
                    && (self.bytes[self.at].is_ascii_alphanumeric()
                        || matches!(self.bytes[self.at], b'_' | b'.'))
                {
                    self.at += 1;
                }
                Token::Other
            }
            b'=' => {
                self.at += 1;
                Token::Equals
            }
            _ => {
                self.at += 1;
                Token::Other
            }
        };
        Some(token)
    }
}

fn tokens(sql: &str, options: &SqlLexOptions) -> Vec<Token> {
    let mut scanner = Scanner::new(sql, options);
    let mut out: Vec<Token> = Vec::new();
    let mut previous_e = false;
    while let Some(token) = scanner.next(previous_e) {
        // `E'...'` is a lone `E` right before the quote (whitespace between
        // them would make it a column named `e` followed by a plain string).
        previous_e = matches!(&token, Token::Word(word) if word.eq_ignore_ascii_case("e"))
            && scanner.peek(0) == Some(b'\'');
        // `U&"..."`: a lone `U` and `&` right before a quoted identifier.
        if let Token::Ident { text, .. } = &token {
            let unicode = matches!(
                out.as_slice(),
                [.., Token::Word(u), Token::Other] if u.eq_ignore_ascii_case("u")
            ) && options.double_quote_is_identifier
                && !options.backtick_identifiers;
            out.push(Token::Ident {
                text: text.clone(),
                unicode,
            });
            continue;
        }
        out.push(token);
    }
    out
}

/// What is wrong with `sql` for an agent, if anything: the offending function
/// (`pg_read_file`) or clause (`INTO OUTFILE`), as written in the text.
pub fn blocked_function(
    sql: &str,
    kind: ConnectionKind,
    options: &SqlLexOptions,
) -> Option<String> {
    let patterns = table(kind);
    if patterns.is_empty() {
        return None;
    }
    let tokens = tokens(sql, options);
    for (index, token) in tokens.iter().enumerate() {
        // Clauses.
        if let Token::Word(word) = token {
            let next = word_at(&tokens, index + 1);
            let upper = word.to_ascii_uppercase();
            let is_clause = match (upper.as_str(), next.as_deref()) {
                ("INTO", Some("OUTFILE" | "DUMPFILE")) => matches!(
                    kind,
                    ConnectionKind::MySql | ConnectionKind::MariaDb | ConnectionKind::ClickHouse
                ),
                ("FROM", Some("INFILE")) => kind == ConnectionKind::ClickHouse,
                ("LOAD", Some("DATA" | "XML")) => {
                    matches!(kind, ConnectionKind::MySql | ConnectionKind::MariaDb)
                        && starts_statement(&tokens, index)
                }
                ("HANDLER", _) => {
                    matches!(kind, ConnectionKind::MySql | ConnectionKind::MariaDb)
                        && starts_statement(&tokens, index)
                }
                _ => false,
            };
            if is_clause {
                return Some(match next {
                    Some(next) if upper != "HANDLER" => format!("{upper} {next}"),
                    _ => upper,
                });
            }
        }
        // Calls: a name followed by `(`.
        let name = match token {
            Token::Word(name) => name.as_str(),
            Token::Ident { text, unicode } => {
                if *unicode && tokens.get(index + 1) == Some(&Token::Open) {
                    return Some(
                        "U&\"…\"(…) (an identifier written with Unicode escapes)".to_owned(),
                    );
                }
                text.as_str()
            }
            _ => continue,
        };
        if tokens.get(index + 1) != Some(&Token::Open) {
            continue;
        }
        if declares_name(&tokens, index) {
            continue;
        }
        if patterns.iter().any(|pattern| glob_matches(pattern, name)) {
            return Some(crate::sanitize::clean_name(
                name,
                crate::sanitize::MAX_NAME_CHARS,
            ));
        }
    }
    None
}

/// A clause agents may not send (`SETTINGS` on ClickHouse), as written.
pub fn unsupported_clause(
    sql: &str,
    kind: ConnectionKind,
    options: &SqlLexOptions,
) -> Option<String> {
    if kind != ConnectionKind::ClickHouse {
        return None;
    }
    let tokens = tokens(sql, options);
    tokens.windows(3).find_map(|window| match window {
        [Token::Word(clause), Token::Word(_) | Token::Ident { .. }, Token::Equals]
            if clause.eq_ignore_ascii_case(CLICKHOUSE_CLAUSE) =>
        {
            Some(CLICKHOUSE_CLAUSE.to_owned())
        }
        _ => None,
    })
}

fn word_at(tokens: &[Token], index: usize) -> Option<String> {
    match tokens.get(index) {
        Some(Token::Word(word)) => Some(word.to_ascii_uppercase()),
        _ => None,
    }
}

/// The token at `index` is the first of its statement.
fn starts_statement(tokens: &[Token], index: usize) -> bool {
    index == 0 || tokens[index - 1] == Token::Semicolon
}

/// `name (` where `name` is declared, not called: a table in `INSERT INTO t (`,
/// `CREATE TABLE t (`, a view, or an alias `AS t (`.
fn declares_name(tokens: &[Token], index: usize) -> bool {
    // Walk back over a qualified name `a . b . name`.
    let mut start = index;
    while start >= 2
        && tokens[start - 1] == Token::Dot
        && matches!(tokens[start - 2], Token::Word(_) | Token::Ident { .. })
    {
        start -= 2;
    }
    let Some(Token::Word(before)) = start.checked_sub(1).and_then(|i| tokens.get(i)) else {
        return false;
    };
    ["TABLE", "INTO", "AS", "VIEW"]
        .iter()
        .any(|keyword| before.eq_ignore_ascii_case(keyword))
}

/// The first word of the first statement, upper-cased (empty if none).
pub fn first_word(sql: &str, options: &SqlLexOptions) -> String {
    tokens(sql, options)
        .into_iter()
        .find_map(|token| match token {
            Token::Word(word) => Some(word.to_ascii_uppercase()),
            Token::Open | Token::Other | Token::Equals | Token::Ident { .. } | Token::Dot => None,
            Token::Semicolon => None,
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ConnectionKind::*;

    fn options(kind: ConnectionKind) -> SqlLexOptions {
        kind.lex_options().unwrap_or(SqlLexOptions::postgres())
    }

    fn hit(kind: ConnectionKind, sql: &str) -> Option<String> {
        blocked_function(sql, kind, &options(kind))
    }

    #[test]
    fn globs() {
        assert!(glob_matches("dblink*", "DBLINK_exec"));
        assert!(!glob_matches(
            "pg_advisory_*lock*",
            "pg_try_advisory_xact_lock"
        ));
        assert!(glob_matches(
            "pg_try_advisory_*",
            "pg_try_advisory_xact_lock"
        ));
        assert!(glob_matches(
            "pg_advisory_*lock*",
            "pg_advisory_xact_lock_shared"
        ));
        assert!(glob_matches("pg_advisory_*lock*", "pg_advisory_lock"));
        assert!(glob_matches("url", "URL"));
        assert!(!glob_matches("url", "urls"));
    }

    #[test]
    fn every_listed_function_is_refused_in_every_position() {
        let samples: &[(ConnectionKind, &[&str])] = &[
            (
                ClickHouse,
                &[
                    "url",
                    "urlCluster",
                    "file",
                    "fileCluster",
                    "remote",
                    "remoteSecure",
                    "cluster",
                    "clusterAllReplicas",
                    "s3",
                    "s3Cluster",
                    "gcs",
                    "azureBlobStorage",
                    "hdfs",
                    "hdfsCluster",
                    "mysql",
                    "postgresql",
                    "mongodb",
                    "redis",
                    "sqlite",
                    "odbc",
                    "jdbc",
                    "executable",
                    "input",
                    "deltaLake",
                    "iceberg",
                    "hudi",
                ],
            ),
            (
                PostgreSql,
                &[
                    "pg_read_file",
                    "pg_read_binary_file",
                    "pg_ls_dir",
                    "pg_ls_logdir",
                    "pg_ls_waldir",
                    "pg_stat_file",
                    "lo_import",
                    "lo_export",
                    "dblink",
                    "dblink_exec",
                    "dblink_connect_u",
                    "query_to_xml",
                    "query_to_xml_and_xmlschema",
                    "cursor_to_xml",
                    "pg_terminate_backend",
                    "pg_cancel_backend",
                    "pg_reload_conf",
                    "pg_rotate_logfile",
                    "set_config",
                    "nextval",
                    "setval",
                    "pg_advisory_lock",
                    "pg_advisory_xact_lock",
                    "pg_advisory_unlock_all",
                    "pg_try_advisory_lock",
                    "pg_try_advisory_xact_lock_shared",
                    "pg_logical_emit_message",
                    "pg_create_restore_point",
                    "pg_create_logical_replication_slot",
                    "pg_drop_replication_slot",
                    "pg_replication_origin_create",
                    "pg_notify",
                ],
            ),
            (
                MySql,
                &[
                    "load_file",
                    "sys_exec",
                    "sys_eval",
                    "get_lock",
                    "release_lock",
                    "benchmark",
                ],
            ),
            (
                MariaDb,
                &[
                    "load_file",
                    "sys_exec",
                    "sys_eval",
                    "get_lock",
                    "release_lock",
                    "benchmark",
                ],
            ),
            (
                Sqlite,
                &[
                    "load_extension",
                    "readfile",
                    "writefile",
                    "edit",
                    "fts3_tokenizer",
                ],
            ),
        ];
        for (kind, names) in samples {
            for name in *names {
                let shapes = [
                    format!("SELECT {name}('x')"),
                    format!("SELECT * FROM {name}('x')"),
                    format!("SELECT * FROM t JOIN {name}('x') AS j ON true"),
                    format!("WITH c AS (SELECT {name}('x')) SELECT * FROM c"),
                    format!("SELECT * FROM (SELECT {name}('x')) s"),
                    format!("SELECT 1 WHERE EXISTS (SELECT 1 FROM {name} ('x'))"),
                    format!("SELECT {}('x')", name.to_ascii_uppercase()),
                    format!("SELECT {}('x')", name.to_ascii_lowercase()),
                    format!("SELECT {name} /* c */ ('x')"),
                    format!("SELECT {name}\n('x')"),
                    format!("SELECT main.{name}('x')"),
                    format!("SELECT pg_catalog.public.{name}('x')"),
                ];
                for sql in shapes {
                    let found = hit(*kind, &sql);
                    assert!(found.is_some(), "{kind:?}: {sql}");
                }
                if matches!(kind, PostgreSql | Sqlite | ClickHouse) {
                    assert!(
                        hit(*kind, &format!("SELECT \"{name}\"('x')")).is_some(),
                        "{kind:?} {name} double quotes"
                    );
                }
                if matches!(kind, MySql | MariaDb | Sqlite | ClickHouse) {
                    assert!(
                        hit(*kind, &format!("SELECT `{name}`('x')")).is_some(),
                        "{kind:?} {name} backticks"
                    );
                }
            }
        }
    }

    #[test]
    fn quoted_and_qualified_forms_are_refused() {
        assert!(hit(PostgreSql, "SELECT \"pg_read_file\"('/etc/passwd')").is_some());
        assert!(hit(
            PostgreSql,
            "SELECT pg_catalog.\"pg_read_file\"('/etc/passwd')"
        )
        .is_some());
        assert!(hit(
            PostgreSql,
            "SELECT \"pg_catalog\".pg_read_file('/etc/passwd')"
        )
        .is_some());
        assert!(hit(PostgreSql, "SELECT U&\"pg\\005fread_file\"('/etc/passwd')").is_some());
        assert!(hit(Sqlite, "SELECT [load_extension]('x')").is_some());
        assert!(hit(Sqlite, "SELECT \"load_extension\"('x')").is_some());
        assert!(hit(
            ClickHouse,
            "SELECT * FROM `url`('http://169.254.169.254/latest')"
        )
        .is_some());
        assert!(hit(
            ClickHouse,
            "SELECT * FROM system.numbers WHERE x IN (SELECT * FROM url('http://a', CSV))"
        )
        .is_some());
        assert!(hit(MySql, "SELECT /*!50000 load_file('/etc/passwd') */").is_some());
        assert!(hit(MariaDb, "SELECT /*M!100000 load_file('/etc/passwd') */").is_some());
    }

    #[test]
    fn clauses_are_refused() {
        assert_eq!(
            hit(MySql, "SELECT 1 INTO OUTFILE '/tmp/x'").as_deref(),
            Some("INTO OUTFILE")
        );
        assert_eq!(
            hit(MariaDb, "SELECT 1 INTO DUMPFILE '/tmp/x'").as_deref(),
            Some("INTO DUMPFILE")
        );
        assert_eq!(
            hit(ClickHouse, "SELECT 1 INTO OUTFILE 'x'").as_deref(),
            Some("INTO OUTFILE")
        );
        assert_eq!(
            hit(ClickHouse, "INSERT INTO t FROM INFILE 'x'").as_deref(),
            Some("FROM INFILE")
        );
        assert_eq!(
            hit(MySql, "LOAD DATA INFILE '/etc/passwd' INTO TABLE t").as_deref(),
            Some("LOAD DATA")
        );
        assert_eq!(
            hit(MySql, "LOAD DATA LOCAL INFILE 'x' INTO TABLE t").as_deref(),
            Some("LOAD DATA")
        );
        assert_eq!(
            hit(MySql, "load  data local infile 'x' into table t").as_deref(),
            Some("LOAD DATA")
        );
        assert_eq!(hit(MySql, "HANDLER t OPEN").as_deref(), Some("HANDLER"));
        assert_eq!(
            hit(MariaDb, "HANDLER t READ FIRST").as_deref(),
            Some("HANDLER")
        );
        // `handler` as a column or table name is fine.
        assert_eq!(hit(MySql, "SELECT handler FROM t"), None);
        assert_eq!(hit(MySql, "SELECT 1 FROM handler"), None);
        assert_eq!(
            hit(PostgreSql, "SELECT 1 INTO OUTFILE"),
            None,
            "PostgreSQL has no OUTFILE clause"
        );
    }

    #[test]
    fn strings_comments_and_dollar_quotes_never_hide_or_fake_a_call() {
        assert_eq!(hit(PostgreSql, "SELECT 'pg_read_file(''x'')'"), None);
        assert_eq!(hit(PostgreSql, "SELECT 1 -- pg_read_file('x')"), None);
        assert_eq!(hit(PostgreSql, "SELECT 1 /* pg_read_file('x') */"), None);
        assert_eq!(
            hit(
                PostgreSql,
                "SELECT 1 /* a /* nested */ pg_read_file('x') */"
            ),
            None
        );
        assert_eq!(hit(PostgreSql, "SELECT $$ pg_read_file('x') $$"), None);
        assert_eq!(hit(PostgreSql, "SELECT $t$ pg_read_file('x') $t$"), None);
        assert_eq!(
            hit(PostgreSql, "SELECT E'\\'' , pg_read_file('x')").as_deref(),
            Some("pg_read_file")
        );
        assert_eq!(
            hit(PostgreSql, "SELECT E'\\' pg_read_file(' ").as_deref(),
            None
        );
        assert_eq!(
            hit(PostgreSql, "SELECT '' || pg_read_file('x')").as_deref(),
            Some("pg_read_file")
        );
        assert_eq!(hit(MySql, "SELECT 'a\\' load_file(' , 1"), None);
        assert_eq!(hit(MySql, "SELECT 1 # load_file('x')"), None);
        assert_eq!(hit(MySql, "SELECT 1 -- load_file('x')"), None);
        assert_eq!(
            hit(MySql, "SELECT 1--load_file('x')").as_deref(),
            Some("load_file")
        );
        assert_eq!(hit(Sqlite, "SELECT 'load_extension(1)'"), None);
        assert_eq!(hit(ClickHouse, "SELECT 'url(x)', \"url(x)\""), None);
        // A comment between the name and the parenthesis does not hide the call,
        // and a quote after a comment starts a string again.
        assert_eq!(
            hit(PostgreSql, "SELECT pg_read_file/**/('x')").as_deref(),
            Some("pg_read_file")
        );
        assert_eq!(
            hit(PostgreSql, "SELECT 'a' /* ' */ , pg_read_file('x')").as_deref(),
            Some("pg_read_file")
        );
    }

    #[test]
    fn names_that_are_not_calls_are_not_refused() {
        for (kind, sql) in [
            (ClickHouse, "SELECT url, file FROM t"),
            (ClickHouse, "SELECT file FROM t WHERE url = 'x'"),
            (ClickHouse, "SELECT * FROM url"),
            (ClickHouse, "SELECT * FROM s3 AS file"),
            (PostgreSql, "SELECT nextval FROM t"),
            (PostgreSql, "SELECT * FROM dblink"),
            (PostgreSql, "SELECT set_config FROM t"),
            (PostgreSql, "INSERT INTO file (a, b) VALUES (1, 2)"),
            (PostgreSql, "CREATE TABLE url (a int)"),
            (PostgreSql, "SELECT * FROM t AS file (a, b)"),
            (
                PostgreSql,
                "SELECT pg_size_pretty(1), pg_sleep(0), now(), length('x')",
            ),
            (MySql, "SELECT sleep(1), now(), concat('a', 'b')"),
            (MySql, "SELECT benchmark FROM t"),
            (Sqlite, "SELECT edit FROM t WHERE readfile = 1"),
            (Sqlite, "SELECT length(x), lower(y) FROM t"),
        ] {
            assert_eq!(hit(kind, sql), None, "{kind:?}: {sql}");
        }
        // A table function in another dialect's list is not that dialect's concern.
        assert_eq!(hit(PostgreSql, "SELECT * FROM url('x')"), None);
        assert_eq!(hit(Redis, "GET url(x)"), None);
    }

    #[test]
    fn the_function_is_named_in_the_result() {
        assert_eq!(
            hit(PostgreSql, "SELECT PG_READ_FILE('x')").as_deref(),
            Some("PG_READ_FILE")
        );
        assert_eq!(
            hit(ClickHouse, "SELECT * FROM s3Cluster('a')").as_deref(),
            Some("s3Cluster")
        );
    }

    #[test]
    fn first_words() {
        let pg = options(PostgreSql);
        assert_eq!(first_word("  /* x */ -- y\n set role admin", &pg), "SET");
        assert_eq!(first_word("(select 1)", &pg), "SELECT");
        assert_eq!(first_word("", &pg), "");
    }

    #[test]
    fn family_globs_and_added_names_are_refused() {
        let cases: &[(ConnectionKind, &[&str])] = &[
            (
                ClickHouse,
                &[
                    "iceberg",
                    "icebergS3",
                    "icebergS3Cluster",
                    "icebergHDFS",
                    "icebergAzure",
                    "deltaLake",
                    "deltaLakeCluster",
                    "deltaLakeS3",
                    "hudi",
                    "hudiCluster",
                    "azureBlobStorage",
                    "azureBlobStorageCluster",
                    "s3",
                    "s3Cluster",
                    "gcs",
                    "hdfs",
                    "hdfsCluster",
                    "remote",
                    "remoteSecure",
                    "cluster",
                    "clusterAllReplicas",
                    "hive",
                    "dictGet",
                    "dictGetOrDefault",
                    "dictGetString",
                    "dictGetUInt64",
                    "mergeTreeIndex",
                    "mysql",
                    "postgresql",
                    "mongodb",
                    "redis",
                    "sqlite",
                    "odbc",
                    "jdbc",
                    "executable",
                    "input",
                    "ytsaurus",
                    "nats",
                    "rabbitmq",
                    "kafka",
                    "url",
                    "urlCluster",
                    "file",
                    "fileCluster",
                ],
            ),
            (
                PostgreSql,
                &[
                    "pg_file_write",
                    "pg_file_unlink",
                    "pg_file_rename",
                    "pg_file_sync",
                    "pg_logdir_ls",
                    "pg_promote",
                    "pg_switch_wal",
                    "pg_stat_reset",
                    "pg_stat_reset_shared",
                    "pg_stat_reset_single_table_counters",
                    "pg_stat_reset_slru",
                    "pg_ls_tmpdir",
                    "pg_ls_archive_statusdir",
                    "pg_current_logfile",
                    "lo_from_bytea",
                    "lo_put",
                    "lo_unlink",
                    "lo_create",
                    "lo_creat",
                ],
            ),
        ];
        for (kind, names) in cases {
            for name in *names {
                for sql in [
                    format!("SELECT {name}('x')"),
                    format!("SELECT * FROM {name}('x')"),
                    format!("SELECT {}('x')", name.to_ascii_uppercase()),
                ] {
                    assert!(hit(*kind, &sql).is_some(), "{kind:?}: {sql}");
                }
            }
        }
    }

    #[test]
    fn harmless_built_ins_next_to_the_globs_are_not_refused() {
        for sql in [
            // `file*` / `url*` would catch these; the list uses exact names instead.
            "SELECT filesystemAvailable(), filesystemCapacity(), filesystemUnreserved()",
            "SELECT URLHash('http://a/b'), URLHierarchy('http://a/b'), URLPathHierarchy('/a/b')",
            "SELECT domain('http://a/b'), cutURLParameter('http://a/?x=1', 'x'), path('http://a/b')",
            "SELECT fileSize, urls FROM t",
            "SELECT dictionaries FROM system.dictionaries",
            "SELECT toString(1), now(), count(), sum(x), hex(x), dictHas('d', 1), dictIsIn('d', 1, 2)",
            "SELECT inputs, remotes, clusters FROM t",
        ] {
            assert_eq!(hit(ClickHouse, sql), None, "{sql}");
        }
        assert_eq!(hit(PostgreSql, "SELECT pg_file_settings FROM t"), None);
        assert_eq!(
            hit(PostgreSql, "SELECT pg_size_pretty(1), pg_current_wal_lsn(), pg_stat_get_numscans(1), lo_get(1), pg_relation_size('t')"),
            None
        );
        assert_eq!(
            hit(
                PostgreSql,
                "SELECT pg_is_in_recovery(), pg_postmaster_start_time(), current_setting('x')"
            ),
            None
        );
    }

    #[test]
    fn a_clickhouse_settings_clause_is_an_unsupported_clause() {
        let ch = options(ClickHouse);
        let clause = |sql: &str| unsupported_clause(sql, ClickHouse, &ch);
        for sql in [
            "SELECT 1 SETTINGS max_threads = 1",
            "select * from t settings readonly=0",
            "SELECT 1 SETTINGS `max_threads`=1, max_memory_usage = 5",
            "SELECT * FROM numbers(10) SETTINGS\n  max_block_size = 1",
            "INSERT INTO t SETTINGS async_insert=1 VALUES (1)",
        ] {
            assert_eq!(clause(sql).as_deref(), Some("SETTINGS"), "{sql}");
        }
        for sql in [
            "SELECT settings FROM t",
            "SELECT settings, x FROM system.table_engines",
            "SELECT 'SETTINGS a = 1'",
            "SELECT 1 -- SETTINGS a = 1",
            "SELECT 1 /* SETTINGS a = 1 */",
            "SELECT t.settings = 1 FROM t",
            "SELECT * FROM settings WHERE a = 1",
        ] {
            assert_eq!(clause(sql), None, "{sql}");
        }
        // Other dialects have no such clause to refuse.
        assert_eq!(
            unsupported_clause("SELECT 1 SETTINGS a = 1", MySql, &options(MySql)),
            None
        );
    }

    #[test]
    fn a_carriage_return_ends_a_line_comment_on_postgres_only() {
        // PostgreSQL ends `--x` at the `\r`: the call after it is code, not comment.
        assert_eq!(
            hit(PostgreSql, "SELECT 1 --x\r, pg_read_file('/etc/passwd')").as_deref(),
            Some("pg_read_file")
        );
        assert_eq!(
            hit(PostgreSql, "SELECT 1 --x\rFROM dblink('a', 'b') t").as_deref(),
            Some("dblink")
        );
        // The others run the comment to the line feed, so there it is all comment.
        assert_eq!(hit(Sqlite, "SELECT 1 --x\r, readfile('/etc/passwd')"), None);
        assert_eq!(hit(ClickHouse, "SELECT 1 --x\r, file('/etc/passwd')"), None);
        assert_eq!(hit(MySql, "SELECT 1 -- x\r, load_file('/etc/passwd')"), None);
        assert_eq!(hit(MySql, "SELECT 1 #x\r, load_file('/etc/passwd')"), None);
    }

    #[test]
    fn mysql_starts_a_dash_comment_before_whitespace_or_a_control_character() {
        // The kit's reading: a control byte after `--` starts the comment too.
        assert_eq!(hit(MySql, "SELECT 1 --\x01 load_file('/etc/passwd')\n"), None);
        assert_eq!(
            hit(MySql, "SELECT 1 --\x01 x\n, load_file('/etc/passwd')").as_deref(),
            Some("load_file")
        );
        // A letter after `--` is not a comment: the call is code.
        assert_eq!(
            hit(MySql, "SELECT 1 --x\n, load_file('/etc/passwd')").as_deref(),
            Some("load_file")
        );
    }
}
