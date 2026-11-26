//! What a pinned session may do with `USE` and `SET`.
//!
//! The shared session of a connection refuses every statement that changes session state, because
//! each call is one self-contained statement. A pinned session keeps its own connection, so a
//! `USE` and a few `SET`s are the point of having it. They are let through by an **allow-list**,
//! never by a deny-list: a setting that is not named here is refused, and so is every statement
//! that could raise the session's own privileges or leave tablex's policy:
//!
//! * the identity of the session: `SET ROLE`, `SET SESSION AUTHORIZATION`, `RESET ROLE`, `SET
//!   DEFAULT ROLE`, `SET PASSWORD`;
//! * the read-only mode and the transaction mode: `default_transaction_read_only`,
//!   `transaction_read_only`, `SET SESSION CHARACTERISTICS`, `SET [SESSION] TRANSACTION ...`,
//!   `autocommit`;
//! * what leaves the session: `SET GLOBAL`, `SET PERSIST`, `SET PERSIST_ONLY`, `@@global.`;
//! * what changes what the server does with the statement: `sql_log_bin`, `sql_mode`,
//!   `foreign_key_checks`, `unique_checks`, `row_security`, `session_replication_role`, ClickHouse
//!   `readonly`, `allow_ddl` and `allow_introspection_functions`.
//!
//! The judgement reads the tokens of the shared SQL lexer with the dialect of the connection's own
//! session (nested comments, `--`, backslashes, `ANSI_QUOTES`: as the server reads them), not the
//! database's grammar: when the text is not one of the shapes below, it is refused. On MySQL and
//! MariaDB every part of a `SET` (the parts after `NAMES` too) is judged, a value is a literal and
//! never an expression (a function in it can be a stored function that writes), and a character set
//! is one of `utf8`, `utf8mb3`, `utf8mb4` and `latin1`.

use crate::ports::ConnectionKind;
use engine_driver_kit::sql::{self, SqlLexOptions, SqlTokenKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// `USE <database>`: the database it names, as written (a database name can be case
    /// sensitive).
    Use(String),
    /// A `SET` or `RESET` of settings that are all on the allow-list.
    Setting,
    /// Not allowed. `escalation`: it would change identity, privilege or the read-only mode (the
    /// model is told not to retry), not only a setting that is not on the list.
    Refused { escalation: bool, message: String },
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    /// A bare word, a number, `@var` or a dotted name: lower case, and as it was written.
    Word(String, String),
    /// A string or a quoted name, without its quotes.
    Quoted(String),
    Symbol(char),
}

/// What `raw`, a quoted token of the lexer (quotes included), says.
fn unquote(raw: &str, lex: &SqlLexOptions) -> String {
    let mut chars = raw.chars();
    let quote = match chars.next() {
        Some(quote @ ('\'' | '"' | '`')) => quote,
        _ => return raw.to_owned(),
    };
    let backslash = match quote {
        '\'' => lex.backslash_escapes,
        '"' if lex.double_quote_is_identifier => lex.backslash_in_identifiers,
        '"' => lex.backslash_escapes,
        _ => lex.backslash_in_identifiers,
    };
    let mut text = String::new();
    while let Some(ch) = chars.next() {
        if backslash && ch == '\\' {
            text.extend(chars.next());
        } else if ch == quote {
            if chars.clone().next() != Some(quote) {
                break;
            }
            chars.next();
            text.push(quote);
        } else {
            text.push(ch);
        }
    }
    text
}

/// The tokens of `sql` as the server reads it (`lex` is the connection's dialect, with what the
/// session reported: `NO_BACKSLASH_ESCAPES`, `ANSI_QUOTES`), by the lexer the classifier uses. A
/// word, a number, `@var` and a dotted name that touch are one word. A trailing `;` is dropped.
/// `None`: the text ends inside a string or a comment.
fn tokens(sql: &str, lex: &SqlLexOptions) -> Option<Vec<Token>> {
    let scan = sql::tokens(sql, lex);
    if scan.unterminated {
        return None;
    }
    let mut tokens: Vec<Token> = Vec::new();
    let mut previous_end = None;
    for token in &scan.tokens {
        let text = &sql[token.range.clone()];
        let wordish = matches!(token.kind, SqlTokenKind::Word | SqlTokenKind::Number)
            || (token.kind == SqlTokenKind::Other && matches!(text, "@" | "."));
        if wordish {
            if let (Some(Token::Word(lower, raw)), true) = (tokens.last_mut(), previous_end == Some(token.range.start)) {
                lower.push_str(&text.to_lowercase());
                raw.push_str(text);
            } else {
                tokens.push(Token::Word(text.to_lowercase(), text.to_owned()));
            }
        } else if token.kind == SqlTokenKind::Quoted {
            tokens.push(Token::Quoted(unquote(text, lex)));
        } else {
            tokens.push(Token::Symbol(text.chars().next().unwrap_or(' ')));
        }
        previous_end = Some(token.range.end);
    }
    while tokens.last() == Some(&Token::Symbol(';')) {
        tokens.pop();
    }
    Some(tokens)
}

fn word(token: Option<&Token>) -> Option<&str> {
    match token {
        Some(Token::Word(word, _)) => Some(word),
        _ => None,
    }
}

fn refuse(escalation: bool, message: impl Into<String>) -> Verdict {
    Verdict::Refused {
        escalation,
        message: message.into(),
    }
}

/// Names that change identity, privilege or the read-only mode, and why.
fn escalation_reason(name: &str) -> Option<&'static str> {
    Some(match name {
        "role" | "session_authorization" | "session.authorization" => {
            "changes the role or the identity the session runs as"
        }
        "default_transaction_read_only" | "transaction_read_only" | "tx_read_only" => {
            "changes the read-only mode of the session"
        }
        "default_transaction_isolation" | "transaction_isolation" | "tx_isolation" => {
            "changes the transaction mode of the session"
        }
        "autocommit" => "changes the transaction mode of the session",
        "sql_log_bin" => "changes what the server logs and replicates",
        "sql_mode" => "changes how the server reads the statements that follow",
        "foreign_key_checks" | "unique_checks" => "turns off integrity checks the data relies on",
        "session_replication_role" | "row_security" => "changes which rules and triggers apply to the session",
        "readonly" | "allow_ddl" | "allow_introspection_functions" => {
            "changes what the session is allowed to do"
        }
        _ => return None,
    })
}

const POSTGRES: &[&str] = &[
    "search_path",
    "timezone",
    "datestyle",
    "intervalstyle",
    "bytea_output",
    "client_min_messages",
    "lock_timeout",
    "statement_timeout",
];

const MYSQL: &[&str] = &[
    "time_zone",
    "character_set_client",
    "character_set_connection",
    "character_set_results",
    "collation_connection",
    "lock_wait_timeout",
    "innodb_lock_wait_timeout",
    "max_execution_time",
    "max_statement_time",
    "group_concat_max_len",
    "sql_select_limit",
    "div_precision_increment",
    "lc_time_names",
];

const CLICKHOUSE: &[&str] = &[
    "session_timezone",
    "date_time_input_format",
    "date_time_output_format",
    "join_use_nulls",
    "max_execution_time",
    "lock_acquire_timeout",
];

fn allowed_list(kind: ConnectionKind) -> &'static [&'static str] {
    match kind {
        ConnectionKind::PostgreSql => POSTGRES,
        ConnectionKind::MySql | ConnectionKind::MariaDb => MYSQL,
        ConnectionKind::ClickHouse => CLICKHOUSE,
        _ => &[],
    }
}

fn setting(kind: ConnectionKind, name: &str, statement: &str) -> Verdict {
    if let Some(why) = escalation_reason(name) {
        return refuse(
            true,
            format!("`{statement}` is refused: it {why}. A pinned session never changes identity, privilege or the read-only mode"),
        );
    }
    if allowed_list(kind).contains(&name) {
        return Verdict::Setting;
    }
    refuse(
        false,
        format!(
            "`{statement}` is not one of the settings a pinned session may change ({}); `USE <database>` also works where the database has it",
            allowed_list(kind).join(", ")
        ),
    )
}

/// The first words of the statement, for messages.
fn head(tokens: &[Token]) -> String {
    tokens
        .iter()
        .take(3)
        .filter_map(|token| match token {
            Token::Word(word, _) => Some(word.to_uppercase()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether `sql` (one statement whose first word is a session word) may run in a pinned session
/// of a connection of `kind`, whose lexer settings are `lex` (what the session reported, so a
/// comment, a string or a nested comment ends where the server ends it).
pub(crate) fn judge(kind: ConnectionKind, sql: &str, lex: &SqlLexOptions) -> Verdict {
    // MySQL and MariaDB run what is inside `/*! ... */` and `/*M! ... */`: a word there is code.
    if sql.contains("/*!") || sql.contains("/*M!") {
        return refuse(false, "a versioned comment (`/*! ... */`) is not available in a pinned session");
    }
    let Some(tokens) = tokens(sql, lex) else {
        return refuse(false, "the statement ends inside a string, a quoted name or a comment");
    };
    // One statement: a `;` inside it would start a second one the judgement never read.
    if tokens.contains(&Token::Symbol(';')) {
        return refuse(false, "a pinned session runs one statement at a time");
    }
    let first = word(tokens.first()).unwrap_or_default();
    match (kind, first) {
        (ConnectionKind::PostgreSql, "use") => {
            refuse(false, "PostgreSQL has no USE: pass `database` to the statements of the session")
        }
        (ConnectionKind::MySql | ConnectionKind::MariaDb | ConnectionKind::ClickHouse, "use") => {
            match tokens.as_slice() {
                [_, Token::Word(_, name) | Token::Quoted(name)] if !name.is_empty() => Verdict::Use(name.clone()),
                _ => refuse(false, "USE takes one database name"),
            }
        }
        (ConnectionKind::PostgreSql, "set") => judge_postgres_set(&tokens),
        (ConnectionKind::PostgreSql, "reset") => judge_postgres_reset(&tokens),
        (ConnectionKind::MySql | ConnectionKind::MariaDb, "set") => judge_mysql_set(kind, &tokens),
        (ConnectionKind::ClickHouse, "set") => judge_clickhouse_set(&tokens),
        _ => refuse(
            false,
            format!(
                "`{}` is not available in a pinned session: only USE and a few SET statements are",
                head(&tokens)
            ),
        ),
    }
}

fn judge_postgres_set(tokens: &[Token]) -> Verdict {
    let statement = head(tokens);
    let mut rest = &tokens[1..];
    while matches!(word(rest.first()), Some("session" | "local")) {
        match word(rest.get(1)) {
            Some("authorization") => return setting(ConnectionKind::PostgreSql, "session_authorization", &statement),
            Some("characteristics") => return setting(ConnectionKind::PostgreSql, "transaction_read_only", &statement),
            _ => rest = &rest[1..],
        }
    }
    match word(rest.first()) {
        Some("transaction") => return setting(ConnectionKind::PostgreSql, "transaction_isolation", &statement),
        Some("time") if word(rest.get(1)) == Some("zone") => {
            return if rest.len() > 2 {
                Verdict::Setting
            } else {
                refuse(false, "SET TIME ZONE takes a value")
            };
        }
        _ => {}
    }
    let Some(name) = word(rest.first()) else {
        return refuse(false, format!("`{statement}` is not a SET of a setting"));
    };
    let name = name.rsplit('.').next().unwrap_or(name);
    // The ones that raise privilege are refused as such whatever follows the name.
    if escalation_reason(name).is_some() {
        return setting(ConnectionKind::PostgreSql, name, &format!("SET {name}"));
    }
    // `SET name TO value` or `SET name = value`; a value is required.
    let assigned = matches!(rest.get(1), Some(Token::Word(w, _)) if w == "to") || rest.get(1) == Some(&Token::Symbol('='));
    if !assigned || rest.len() < 3 {
        return refuse(false, format!("`SET {name}` takes `TO value` or `= value`"));
    }
    setting(ConnectionKind::PostgreSql, name, &format!("SET {name}"))
}

fn judge_postgres_reset(tokens: &[Token]) -> Verdict {
    let statement = head(tokens);
    match word(tokens.get(1)) {
        Some("all") => refuse(
            true,
            "`RESET ALL` is refused: it puts back every setting, including the ones tablex relies on; reset the settings one by one",
        ),
        Some("session") => setting(ConnectionKind::PostgreSql, "session_authorization", &statement),
        Some(name) if tokens.len() == 2 => {
            let name = name.rsplit('.').next().unwrap_or(name);
            setting(ConnectionKind::PostgreSql, name, &format!("RESET {name}"))
        }
        _ => refuse(false, format!("`{statement}` is not a RESET of one setting")),
    }
}

/// The scope words that can start an assignment of MySQL's `SET`.
fn mysql_target(tokens: &[Token]) -> (&[Token], Option<&'static str>) {
    let mut rest = tokens;
    let mut scope = None;
    while let Some(scope_word) = word(rest.first()) {
        match scope_word {
            "global" | "persist" | "persist_only" => scope = Some("global"),
            "session" | "local" => {}
            _ => break,
        }
        rest = &rest[1..];
    }
    (rest, scope)
}

/// The character sets a pinned session may select: a multi-byte set whose trail bytes can be a
/// backslash or a quote (`gbk`, `sjis`, `big5`) changes where a string ends for the server and not
/// for tablex.
const CHARSETS: &[&str] = &["utf8", "utf8mb3", "utf8mb4", "latin1"];

fn charset_name(token: Option<&Token>) -> Option<&str> {
    match token {
        Some(Token::Word(lower, _)) => Some(lower.as_str()),
        Some(Token::Quoted(text)) => Some(text.as_str()),
        _ => None,
    }
    .filter(|name| CHARSETS.contains(&name.to_lowercase().as_str()))
}

fn charset_refusal(statement: &str) -> Verdict {
    refuse(
        false,
        format!(
            "`{statement}` is refused: a pinned session may only select the character sets {}",
            CHARSETS.join(", ")
        ),
    )
}

/// `NAMES x [COLLATE y]`, `CHARACTER SET x` or `CHARSET x`, one part of a `SET`: `rest` is what
/// follows the words that name the part.
fn judge_charset_part(statement: &str, rest: &[Token], collate: bool) -> Verdict {
    let set = charset_name(rest.first()).is_some();
    let tail_ok = match rest {
        [_] => true,
        [_, Token::Word(word, _), name] if collate && word == "collate" => collation_charset_ok(name),
        _ => false,
    };
    if set && tail_ok {
        Verdict::Setting
    } else {
        charset_refusal(statement)
    }
}

/// A collation name (`utf8mb4_unicode_ci`) of one of the allowed character sets.
fn collation_charset_ok(token: &Token) -> bool {
    let name = match token {
        Token::Word(lower, _) => lower.as_str(),
        Token::Quoted(text) => &text.to_lowercase(),
        _ => return false,
    };
    name.split('_').next().is_some_and(|charset| CHARSETS.contains(&charset))
}

/// The value of an assignment in MySQL and MariaDB. It is a literal and nothing else: a number
/// (with a minus), a string, a word (`ON`, `DEFAULT`, `NULL`, an enum value) or `@variable`. An
/// expression is run by the server, and a function in it can be a stored function that writes: the
/// statement is not judged as the read it looks like (`SET @n = app.next_number()`).
fn literal_value(value: &[Token]) -> bool {
    let number = |word: &str| word.starts_with(|ch: char| ch.is_ascii_digit()) || word.strip_prefix('.').is_some_and(|rest| rest.starts_with(|ch: char| ch.is_ascii_digit()));
    match value {
        [Token::Quoted(_)] => true,
        [Token::Word(word, _)] => number(word) || word.starts_with('@') || !word.contains('.'),
        [Token::Symbol('-'), Token::Word(word, _)] => number(word),
        _ => false,
    }
}

fn judge_mysql_set(kind: ConnectionKind, tokens: &[Token]) -> Verdict {
    let statement = head(tokens);
    let body = &tokens[1..];
    match (word(body.first()), word(body.get(1))) {
        (Some("role"), _) | (Some("default"), Some("role")) => {
            return setting(kind, "role", &statement);
        }
        (Some("password"), _) => {
            return refuse(true, "`SET PASSWORD` is refused: it changes the credentials of an account");
        }
        _ => {}
    }
    // `SET TRANSACTION ...` and `SET SESSION TRANSACTION ...` (the read-only mode).
    let (after_scope, scope) = mysql_target(body);
    if word(after_scope.first()) == Some("transaction") {
        return setting(kind, "transaction_read_only", &statement);
    }
    // The parts, split at the commas outside brackets; each names its own scope, and
    // `NAMES` and `CHARACTER SET` are parts like the assignments (more can follow them).
    let mut depth = 0usize;
    let mut segments: Vec<&[Token]> = Vec::new();
    let mut start = 0;
    for (at, token) in body.iter().enumerate() {
        match token {
            Token::Symbol('(') => depth += 1,
            Token::Symbol(')') => depth = depth.saturating_sub(1),
            Token::Symbol(',') if depth == 0 => {
                segments.push(&body[start..at]);
                start = at + 1;
            }
            _ => {}
        }
    }
    segments.push(&body[start..]);
    // The first refusal is kept and the rest is read all the same: a later part that escalates
    // makes it one the model is told not to retry.
    let mut refusal = None;
    for segment in segments {
        match judge_mysql_part(kind, &statement, segment, scope) {
            Verdict::Setting => {}
            Verdict::Refused { escalation: true, message } => return refuse(true, message),
            refused => {
                refusal.get_or_insert(refused);
            }
        }
    }
    refusal.unwrap_or(Verdict::Setting)
}

/// One part of a MySQL `SET`: `NAMES ...`, `CHARACTER SET ...` or one assignment; `scope` is the
/// scope word the whole statement started with.
fn judge_mysql_part(kind: ConnectionKind, statement: &str, segment: &[Token], scope: Option<&'static str>) -> Verdict {
    let part = match (word(segment.first()), word(segment.get(1))) {
        (Some("names"), _) => Some(judge_charset_part(statement, &segment[1..], true)),
        (Some("character"), Some("set")) => Some(judge_charset_part(statement, &segment[2..], false)),
        (Some("charset"), _) => Some(judge_charset_part(statement, &segment[1..], false)),
        _ => None,
    };
    if let Some(part) = part {
        return part;
    }
    let (target, segment_scope) = mysql_target(segment);
    let Some(first) = word(target.first()) else {
        return refuse(false, format!("`{statement}` is not a SET of a setting"));
    };
    let (assigned, value) = match (target.get(1), target.get(2)) {
        (Some(Token::Symbol('=')), _) => (true, &target[2..]),
        (Some(Token::Symbol(':')), Some(Token::Symbol('='))) => (true, &target[3..]),
        _ => (false, &target[..0]),
    };
    if !assigned {
        return refuse(false, format!("`{statement}` is not a SET of a setting"));
    }
    if segment_scope.or(scope).is_some() {
        return refuse(
            true,
            format!("`{statement}` is refused: it changes the server for every connection, not this session"),
        );
    }
    let mut name = None;
    if first.starts_with("@@") {
        let system = first.trim_start_matches("@@");
        let (scope_part, system) = match system.split_once('.') {
            Some((scope_part, system)) => (scope_part, system),
            None => ("session", system),
        };
        if matches!(scope_part, "global" | "persist" | "persist_only") {
            return refuse(
                true,
                format!("`{statement}` is refused: it changes the server for every connection, not this session"),
            );
        }
        name = Some(system);
    } else if !first.starts_with('@') {
        // (A user variable is state of this session and nothing else.)
        name = Some(first);
    }
    if let Some(name) = name {
        match setting(kind, name, &format!("SET {name}")) {
            Verdict::Setting => {}
            refused => return refused,
        }
    }
    if !literal_value(value) {
        return refuse(
            false,
            format!(
                "`{statement}` is refused: on {} a pinned session sets a variable to a literal only (a number, a string, ON, OFF, DEFAULT, NULL or @variable), never an expression, a function or a subquery",
                kind.driver_id()
            ),
        );
    }
    let charset = name.is_some_and(|name| matches!(name, "character_set_client" | "character_set_connection" | "character_set_results"));
    if charset && charset_name(value.first()).is_none() {
        return charset_refusal(statement);
    }
    if name == Some("collation_connection") && !value.first().is_some_and(collation_charset_ok) {
        return charset_refusal(statement);
    }
    Verdict::Setting
}

fn judge_clickhouse_set(tokens: &[Token]) -> Verdict {
    let statement = head(tokens);
    let body = &tokens[1..];
    if matches!(word(body.first()), Some("role")) || matches!((word(body.first()), word(body.get(1))), (Some("default"), Some("role"))) {
        return setting(ConnectionKind::ClickHouse, "role", &statement);
    }
    let mut depth = 0usize;
    let mut segments: Vec<&[Token]> = Vec::new();
    let mut start = 0;
    for (at, token) in body.iter().enumerate() {
        match token {
            Token::Symbol('(' | '[') => depth += 1,
            Token::Symbol(')' | ']') => depth = depth.saturating_sub(1),
            Token::Symbol(',') if depth == 0 => {
                segments.push(&body[start..at]);
                start = at + 1;
            }
            _ => {}
        }
    }
    segments.push(&body[start..]);
    for segment in segments {
        let (Some(name), Some(Token::Symbol('='))) = (word(segment.first()), segment.get(1)) else {
            return refuse(false, format!("`{statement}` is not a SET of a setting"));
        };
        match setting(ConnectionKind::ClickHouse, name, &format!("SET {name}")) {
            Verdict::Setting => {}
            refused => return refused,
        }
    }
    Verdict::Setting
}

#[cfg(test)]
mod tests {
    use super::*;
    use ConnectionKind::*;

    fn lex(kind: ConnectionKind) -> SqlLexOptions {
        kind.lex_options().unwrap_or(SqlLexOptions::postgres())
    }

    fn judge(kind: ConnectionKind, sql: &str) -> Verdict {
        super::judge(kind, sql, &lex(kind))
    }

    fn allowed(kind: ConnectionKind, sql: &str) -> bool {
        matches!(judge(kind, sql), Verdict::Setting | Verdict::Use(_))
    }

    fn escalation(kind: ConnectionKind, sql: &str) -> bool {
        matches!(judge(kind, sql), Verdict::Refused { escalation: true, .. })
    }

    fn refused(kind: ConnectionKind, sql: &str) -> bool {
        matches!(judge(kind, sql), Verdict::Refused { .. })
    }

    #[test]
    fn postgres_allows_the_harmless_settings() {
        for sql in [
            "SET search_path TO app, public",
            "SET search_path = 'app'",
            "SET SESSION search_path = app;",
            "set timezone to 'UTC'",
            "SET TIME ZONE 'Europe/Paris'",
            "SET lock_timeout = '5s'",
            "SET statement_timeout TO 3000",
            "SET DateStyle = 'ISO, DMY'",
            "SET pg_catalog.search_path = x",
            "RESET search_path",
            "RESET lock_timeout",
            "/* why */ SET search_path TO a -- later",
        ] {
            assert!(allowed(PostgreSql, sql), "{sql}: {:?}", judge(PostgreSql, sql));
        }
    }

    #[test]
    fn postgres_refuses_every_way_to_change_identity_privilege_or_the_read_only_mode() {
        for sql in [
            "SET ROLE admin",
            "SET LOCAL ROLE admin",
            "SET SESSION ROLE admin",
            "SET role = 'admin'",
            "RESET ROLE",
            "SET SESSION AUTHORIZATION postgres",
            "SET LOCAL SESSION AUTHORIZATION postgres",
            "RESET SESSION AUTHORIZATION",
            "SET default_transaction_read_only = off",
            "SET SESSION default_transaction_read_only TO false",
            "SET transaction_read_only = off",
            "SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE",
            "SET TRANSACTION READ WRITE",
            "SET default_transaction_isolation = 'serializable'",
            "SET session_replication_role = replica",
            "SET row_security = off",
            "RESET ALL",
        ] {
            assert!(escalation(PostgreSql, sql), "{sql}: {:?}", judge(PostgreSql, sql));
        }
    }

    #[test]
    fn postgres_refuses_what_is_not_on_the_list_and_what_is_not_a_set() {
        for sql in [
            "SET work_mem = '1GB'",
            "SET application_name = 'x'",
            "SET standard_conforming_strings = off",
            "SET search_path",
            "SET search_path TO",
            "SET",
            "RESET",
            "RESET a b",
            "USE shop",
            "DISCARD ALL",
            "SET custom.setting = 1",
            "SET extra_float_digits = 0",
        ] {
            assert!(refused(PostgreSql, sql), "{sql}: {:?}", judge(PostgreSql, sql));
        }
        assert!(matches!(judge(PostgreSql, "USE shop"), Verdict::Refused { escalation: false, .. }));
    }

    #[test]
    fn mysql_allows_use_and_the_harmless_settings() {
        for sql in [
            "SET time_zone = '+00:00'",
            "SET SESSION time_zone = 'UTC'",
            "SET @@session.time_zone = 'UTC'",
            "SET @@time_zone = 'UTC'",
            "SET NAMES utf8mb4",
            "SET NAMES utf8mb4 COLLATE utf8mb4_unicode_ci",
            "SET CHARACTER SET utf8mb4",
            "SET lock_wait_timeout = 5",
            "SET SESSION lock_wait_timeout = 5, time_zone = 'UTC'",
            "SET @x = 5",
            "SET @x := 5, @y = 'a', @z = -1.5, @w = @x, @v = NULL, @u = DEFAULT, @t = ON",
            "SET time_zone = DEFAULT, @@session.lock_wait_timeout = 5",
            "SET NAMES utf8mb4, time_zone = 'UTC'",
            "SET @x = 1, NAMES latin1",
            "SET NAMES 'utf8'",
            "SET character_set_client = utf8mb4, collation_connection = utf8mb4_unicode_ci",
            "SET max_execution_time = 1000",
            "SET group_concat_max_len = 100000",
        ] {
            assert!(allowed(MySql, sql), "{sql}: {:?}", judge(MySql, sql));
            assert!(allowed(MariaDb, sql), "{sql}");
        }
        assert_eq!(judge(MySql, "USE `shop`"), Verdict::Use("shop".into()));
        assert_eq!(judge(MariaDb, "use shop;"), Verdict::Use("shop".into()));
        assert_eq!(judge(MySql, "USE 'my db'"), Verdict::Use("my db".into()));
        assert!(refused(MySql, "USE"));
        assert!(refused(MySql, "USE a b"));
    }

    #[test]
    fn mysql_refuses_every_way_to_change_identity_privilege_or_the_read_only_mode() {
        for sql in [
            "SET SESSION TRANSACTION READ WRITE",
            "SET TRANSACTION READ WRITE",
            "SET SESSION TRANSACTION ISOLATION LEVEL SERIALIZABLE",
            "SET transaction_read_only = 0",
            "SET @@session.transaction_read_only = 0",
            "SET tx_read_only = 0",
            "SET autocommit = 0",
            "SET sql_log_bin = 0",
            "SET SESSION sql_log_bin = OFF",
            "SET sql_mode = ''",
            "SET foreign_key_checks = 0",
            "SET unique_checks = 0",
            "SET GLOBAL max_connections = 1",
            "SET PERSIST max_connections = 1",
            "SET PERSIST_ONLY max_connections = 1",
            "SET @@global.max_connections = 1",
            "SET @@persist.max_connections = 1",
            "SET time_zone = 'UTC', GLOBAL max_connections = 1",
            "SET ROLE admin",
            "SET DEFAULT ROLE admin TO u",
            "SET PASSWORD = 'x'",
            "SET PASSWORD FOR u = 'x'",
        ] {
            assert!(escalation(MySql, sql), "{sql}: {:?}", judge(MySql, sql));
            assert!(escalation(MariaDb, sql), "{sql}");
        }
        for sql in ["SET max_connections = 1", "SET wait_timeout = 1", "SET SQL_SAFE_UPDATES = 0", "SET x", "RESET MASTER", "RESET QUERY CACHE", "FLUSH PRIVILEGES"] {
            assert!(refused(MySql, sql), "{sql}");
        }
    }

    #[test]
    fn clickhouse_allows_use_and_a_few_settings_and_refuses_the_privilege_ones() {
        assert_eq!(judge(ClickHouse, "USE analytics"), Verdict::Use("analytics".into()));
        for sql in [
            "SET session_timezone = 'UTC'",
            "SET join_use_nulls = 1, date_time_output_format = 'iso'",
            "SET max_execution_time = 5",
        ] {
            assert!(allowed(ClickHouse, sql), "{sql}");
        }
        for sql in [
            "SET readonly = 0",
            "SET allow_ddl = 1",
            "SET allow_introspection_functions = 1",
            "SET ROLE admin",
            "SET DEFAULT ROLE admin",
            "SET join_use_nulls = 1, readonly = 0",
        ] {
            assert!(escalation(ClickHouse, sql), "{sql}: {:?}", judge(ClickHouse, sql));
        }
        for sql in ["SET max_threads = 1000", "SET profile = 'x'", "SET x"] {
            assert!(refused(ClickHouse, sql), "{sql}");
        }
    }

    #[test]
    fn other_engines_and_other_statements_have_no_session_state() {
        for kind in [Sqlite, MongoDb, Redis] {
            assert!(refused(kind, "SET x = 1"), "{kind:?}");
            assert!(refused(kind, "USE x"), "{kind:?}");
        }
        for sql in ["BEGIN", "COMMIT", "PRAGMA foreign_keys = off", "LOCK TABLE t", "DO $$ begin end $$", "PREPARE x AS SELECT 1", "LISTEN x"] {
            assert!(refused(PostgreSql, sql), "{sql}");
            assert!(refused(MySql, sql), "{sql}");
        }
    }

    #[test]
    fn comments_and_quotes_do_not_hide_a_word() {
        assert!(escalation(PostgreSql, "SET /* a */ ROLE /* b */ admin"));
        assert!(escalation(PostgreSql, "-- note\nSET ROLE admin"));
        assert!(refused(MySql, "SET /*! x */ GLOBAL max_connections = 1"));
        assert!(refused(MariaDb, "SET /*M!100000 ROLE */ admin"));
        assert!(escalation(MySql, "SET /* x */ GLOBAL max_connections = 1"));
        // A string that says ROLE is just a string.
        assert!(allowed(PostgreSql, "SET search_path TO 'role'"));
        assert!(allowed(MySql, "SET @x = 'GLOBAL'"));
    }

    #[test]
    fn postgres_reads_nested_block_comments_as_the_server_does() {
        // PostgreSQL nests `/* */`: the server runs `SET ROLE admin` here.
        for setting in ["ROLE admin", "SESSION AUTHORIZATION postgres", "default_transaction_read_only = off", "session_replication_role = replica", "row_security = off"] {
            let sql = format!("SET /* /* */ search_path TO x /* */ */ {setting}");
            assert!(escalation(PostgreSql, &sql), "{sql}: {:?}", judge(PostgreSql, &sql));
        }
        // The same text is a comment that ends at the first `*/` for a dialect that does not nest.
        assert!(!escalation(Sqlite, "SET /* /* */ search_path TO x /* */ */ ROLE admin"));
        // What is inside a nested comment is not a word.
        assert!(allowed(PostgreSql, "SET search_path TO a /* /* ROLE */ */"));
        assert!(refused(PostgreSql, "SET search_path TO a /* never closed"));
    }

    #[test]
    fn a_second_statement_is_never_judged_away() {
        for sql in ["SET search_path TO a; SET ROLE admin", "SET search_path TO a;; SELECT 1"] {
            assert!(refused(PostgreSql, sql), "{sql}");
        }
        assert!(refused(MySql, "SET @a = 1; SET GLOBAL max_connections = 1"));
        assert!(allowed(PostgreSql, "SET search_path TO a;"));
    }

    #[test]
    fn mysql_judges_every_part_after_names_and_character_set() {
        for sql in [
            "SET NAMES utf8mb4, foreign_key_checks = 0",
            "SET NAMES utf8mb4, GLOBAL read_only = 0",
            "SET NAMES utf8mb4, sql_log_bin = 0",
            "SET NAMES utf8mb4 COLLATE utf8mb4_bin, @@global.max_connections = 1",
            "SET CHARACTER SET utf8mb4, foreign_key_checks = 0",
            "SET CHARSET utf8mb4, sql_mode = ''",
            "SET @a = 1, NAMES utf8mb4, unique_checks = 0",
        ] {
            assert!(escalation(MySql, sql), "{sql}: {:?}", judge(MySql, sql));
            assert!(escalation(MariaDb, sql), "{sql}");
        }
        for sql in ["SET NAMES utf8mb4, max_connections = 1", "SET NAMES utf8mb4,", "SET NAMES utf8mb4 utf8mb4"] {
            assert!(refused(MySql, sql), "{sql}");
        }
    }

    #[test]
    fn mysql_is_lexed_as_the_server_lexes_it() {
        // `--` without a blank after it is not a comment in MySQL: the `, GLOBAL ...` is code.
        let dash = "SET @a = 1--1, GLOBAL max_connections = 1";
        assert!(escalation(MySql, dash), "{:?}", judge(MySql, dash));
        assert!(escalation(MariaDb, dash));
        // With a blank it is a comment (to the end of the line).
        assert!(allowed(MySql, "SET @a = 1 -- , GLOBAL max_connections = 1"));
        // A backslash escapes inside `"` too: the string runs to the last quote.
        let quote = r#"SET @x = "\" , @y = ", foreign_key_checks = 0, @z = "x""#;
        assert!(escalation(MySql, quote), "{:?}", judge(MySql, quote));
        // NO_BACKSLASH_ESCAPES: `'\'` is a whole string and what follows is code. With backslash
        // escapes the same text is one string that never ends.
        let single = r"SET @x = '\', foreign_key_checks = 0, @z = 'x'";
        let plain = SqlLexOptions::mysql().without_backslash_escapes();
        assert!(matches!(super::judge(MySql, single, &plain), Verdict::Refused { escalation: true, .. }));
        assert!(refused(MySql, single) && !escalation(MySql, single));
        assert!(allowed(MySql, r"SET @x = 'a\'b'"));
        // ANSI_QUOTES: `"x"` is a name, not a string.
        let ansi = SqlLexOptions::mysql().with_ansi_quotes();
        assert!(matches!(super::judge(MySql, r#"SET @x = 1, "foreign_key_checks" = 0"#, &ansi), Verdict::Refused { .. }));
        assert!(matches!(super::judge(MySql, "SET @x = 'a'", &ansi), Verdict::Setting));
        // A string that is not closed is not judged.
        assert!(refused(MySql, "SET @x = 'a"));
    }

    #[test]
    fn mysql_sets_literals_only() {
        for sql in [
            "SET @n = app.next_invoice_no()",
            "SET @n = next_invoice_no()",
            "SET @n = `app`.`f`()",
            "SET @n = (SELECT 1)",
            "SET @n = 1 + 1",
            "SET @n = 1 - 1",
            "SET @n = 1--1",
            "SET @n = - -1",
            "SET @n = @a + 1",
            "SET @n = 'a' 'b'",
            "SET @n = 1, @m = now()",
            "SET @n = (1, GLOBAL max_connections = 1)",
            "SET @n =",
            "SET time_zone = (SELECT 'UTC')",
            "SET lock_wait_timeout = sleep(1)",
            "SET @@session.lock_wait_timeout = f()",
            "SET @n = app.f",
        ] {
            assert!(refused(MySql, sql), "{sql}: {:?}", judge(MySql, sql));
            assert!(refused(MariaDb, sql), "{sql}");
            assert!(!escalation(MySql, sql) || sql.contains("GLOBAL"), "{sql}");
        }
    }

    #[test]
    fn mysql_character_sets_are_utf8_or_latin1() {
        for sql in [
            "SET NAMES gbk",
            "SET NAMES 'sjis'",
            "SET NAMES big5 COLLATE big5_bin",
            "SET NAMES binary",
            "SET NAMES DEFAULT",
            "SET NAMES utf8mb4 COLLATE gbk_chinese_ci",
            "SET CHARACTER SET gbk",
            "SET CHARSET cp932",
            "SET character_set_client = gbk",
            "SET character_set_connection = 'sjis'",
            "SET @@session.character_set_client = gbk",
            "SET character_set_results = binary",
            "SET collation_connection = gbk_bin",
            "SET character_set_client = DEFAULT",
            "SET NAMES",
        ] {
            assert!(refused(MySql, sql), "{sql}: {:?}", judge(MySql, sql));
            assert!(refused(MariaDb, sql), "{sql}");
        }
        for sql in ["SET NAMES utf8", "SET NAMES utf8mb3", "SET NAMES UTF8MB4", "SET CHARSET latin1", "SET character_set_results = utf8mb4"] {
            assert!(allowed(MySql, sql), "{sql}: {:?}", judge(MySql, sql));
        }
    }

    #[test]
    fn use_keeps_the_name_as_written() {
        // A database name can be case sensitive: the scope of a connection compares it as it is.
        assert_eq!(judge(MySql, "USE Shop"), Verdict::Use("Shop".into()));
        assert_eq!(judge(MySql, "USE `Shop`"), Verdict::Use("Shop".into()));
    }
}
