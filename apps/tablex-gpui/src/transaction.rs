//! Manual transactions of a SQL tab: the mode, and the little the app keeps about the transaction
//! the tab's session has open. Pure: the session, the buttons and the guards are
//! `app::transaction`.
//!
//! A tab is in [`CommitMode::Auto`] (its statements run on the connection's shared session and
//! each commits by itself; an explicit transaction a run leaves open is rolled back when the run
//! ends) or [`CommitMode::Manual`] (its statements run on a session of the
//! tab's own, inside a transaction that stays open until the user commits or rolls back).
//!
//! The server owns the transaction state; the app does not infer it from the SQL. MySQL and
//! MariaDB sessions of a Manual tab run with `autocommit = 0` ([`session_setup`]), so every
//! statement is inside a transaction (a DDL statement that commits by itself just starts the next
//! one). PostgreSQL and SQLite get a `BEGIN` before each statement ([`begin_statement`]) whose
//! result is ignored: the server says "already in progress" when one is open. All the app keeps
//! is how many statements were sent since the last Commit /
//! Rollback and how many of them modify ([`TabTx`]): the toolbar shows them, and only modifying
//! ones are asked about before a session is closed.

use crate::connections::ConnectionKind;
use crate::sql_account::{Scrubber, REDACTED_LITERAL};
use engine_driver_kit::sql::{classify, token_iter, tokens, Keyword, SqlLexOptions, SqlToken, SqlTokenKind};
use std::collections::VecDeque;
use std::time::{Duration, Instant, SystemTime};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CommitMode {
    /// Every statement commits by itself, on the connection's shared session.
    #[default]
    Auto,
    /// Statements run on the tab's own session and wait for Commit or Rollback.
    Manual,
}

/// What is sent before each statement of a Manual tab, result ignored. `None`: MySQL and
/// MariaDB, whose session runs with autocommit off, and the kinds without a manual mode.
pub fn begin_statement(kind: ConnectionKind) -> Option<&'static str> {
    matches!(kind, ConnectionKind::Postgres | ConnectionKind::Sqlite).then_some("BEGIN")
}

/// What a tab's own session runs once when it opens (MySQL / MariaDB: no autocommit).
pub fn session_setup(kind: ConnectionKind) -> Option<&'static str> {
    matches!(kind, ConnectionKind::Mysql | ConnectionKind::Mariadb).then_some("SET autocommit = 0")
}

/// Why a tab on a `kind` connection cannot be in manual mode; `None` when it can.
///
/// `read_only`: the connection is read-only, so every statement runs in the driver's read-only
/// scope, which refuses `BEGIN` (it touches transaction state). `in_memory`: a SQLite database
/// that exists for one session only, which a second session would not see.
pub fn manual_unavailable(kind: ConnectionKind, read_only: bool, in_memory: bool) -> Option<&'static str> {
    match kind {
        ConnectionKind::Clickhouse => Some("ClickHouse has no transactions."),
        ConnectionKind::Redis => Some("Redis commands run one at a time; there is no commit mode."),
        ConnectionKind::Mongodb => Some("MongoDB tabs do not use SQL transactions."),
        _ if read_only => Some(
            "This connection is read-only: the server refuses BEGIN in a read-only run. \
             Turn read-only off for its connection type to use manual commit.",
        ),
        ConnectionKind::Sqlite if in_memory => Some(
            "An in-memory SQLite database exists for one session only, so a tab cannot have a \
             session of its own.",
        ),
        _ => None,
    }
}

/// The text may be a `SET autocommit` (a cheap look, before it is read properly).
pub fn mentions_autocommit(sql: &str) -> bool {
    sql.as_bytes().windows(10).any(|window| window.eq_ignore_ascii_case(b"autocommit"))
}

/// The database a tab's `name` stands for on the connection `uri`: an empty name is the
/// connection's default one.
pub fn database_name(kind: ConnectionKind, uri: &str, name: &str) -> String {
    if !name.is_empty() {
        return name.to_owned();
    }
    match kind {
        ConnectionKind::Sqlite => "main".to_owned(),
        _ => uri
            .split_once("://")
            .and_then(|(_, rest)| rest.split_once('/'))
            .map(|(_, path)| path.split(['?', '#']).next().unwrap_or_default().to_owned())
            .unwrap_or_default(),
    }
}

/// Whether two database names of a tab are the same database of the connection `uri`.
pub fn same_database(kind: ConnectionKind, uri: &str, a: &str, b: &str) -> bool {
    database_name(kind, uri, a) == database_name(kind, uri, b)
}

/// What the user typed as the first statement of a run, for the few that are transaction
/// statements themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Typed {
    /// `BEGIN`, `START TRANSACTION`.
    Begin,
    /// `COMMIT`, `END`, `ROLLBACK`, `ABORT`: the whole transaction ends (`ROLLBACK TO` does not).
    End,
    /// `COMMIT AND CHAIN`, `ROLLBACK AND CHAIN`: one transaction ends and the next is open at once.
    Chain,
    /// `COMMIT PREPARED`, `ROLLBACK PREPARED`: sent as typed.
    Prepared,
    /// `PREPARE TRANSACTION`: sent as typed; it takes the session's transaction away (PostgreSQL).
    PrepareTransaction,
    /// `SAVEPOINT`: outside a transaction it opens one on SQLite.
    Savepoint,
    /// `SET autocommit = 0` / `= 1` (MySQL, MariaDB): whether it turns autocommit on.
    Autocommit(bool),
    /// Anything else.
    Other,
}

impl Typed {
    /// What the first statement of `sql` is. Everything after its keyword is read with the lexer the
    /// session reads with (`lex`), so a comment, a string, a quoted name or a user variable that
    /// spells `autocommit` or `PREPARED` is what the server takes it for.
    pub fn of(sql: &str, lex: &SqlLexOptions) -> Self {
        let classification = classify(sql, lex);
        let Some(first) = classification.statements.first() else {
            return Self::Other;
        };
        if first.opens_transaction {
            return Self::Begin;
        }
        let statement = &sql[first.range.clone()];
        let keyword_end = first.keyword_range.as_ref().map_or(0, |range| range.end - first.range.start);
        let mut rest = token_iter(statement, lex).skip_while(|token| token.range.start < keyword_end);
        let next_is = |rest: &mut dyn Iterator<Item = SqlToken>, expected: &str| {
            rest.next().is_some_and(|token| token.kind == SqlTokenKind::Word && statement[token.range].eq_ignore_ascii_case(expected))
        };
        if first.ends_transaction {
            // `AND CHAIN` (not `AND NO CHAIN`) starts the next transaction in the same breath.
            let mut previous_and = false;
            let chains = rest.any(|token| {
                let word = token.kind == SqlTokenKind::Word;
                let text = &statement[token.range];
                let chain = previous_and && word && text.eq_ignore_ascii_case("chain");
                previous_and = word && text.eq_ignore_ascii_case("and");
                chain
            });
            return if chains { Self::Chain } else { Self::End };
        }
        match first.keyword {
            Some(Keyword::Commit | Keyword::Rollback) if next_is(&mut rest, "prepared") => Self::Prepared,
            Some(Keyword::Prepare) if next_is(&mut rest, "transaction") => Self::PrepareTransaction,
            Some(Keyword::Savepoint) => Self::Savepoint,
            Some(Keyword::Set) => autocommit_setting(statement, rest, lex).map_or(Self::Other, Self::Autocommit),
            _ => Self::Other,
        }
    }

    /// The app's `BEGIN` goes before every statement but these.
    pub fn takes_begin(self) -> bool {
        matches!(self, Self::Other | Self::Autocommit(_) | Self::Savepoint)
    }
}

/// The value a `SET` gives the session's `autocommit` (`Some(true)`: on): `autocommit = 0`, `SESSION
/// autocommit := OFF`, `@@session.autocommit = 1`, also among other assignments and with the name
/// quoted. Only a name the server reads as the variable counts: not one inside a string or a comment,
/// not a user variable (`@autocommit`, `@my_autocommit`), and a `"name"` only where `"` quotes names.
fn autocommit_setting(statement: &str, tokens: impl Iterator<Item = SqlToken>, lex: &SqlLexOptions) -> Option<bool> {
    let text = |token: &SqlToken| &statement[token.range.clone()];
    let is_other = |token: &SqlToken, expected: &str| token.kind == SqlTokenKind::Other && text(token) == expected;
    let mut tokens = tokens.peekable();
    let mut at_signs = 0_u32;
    while let Some(token) = tokens.next() {
        let named = match token.kind {
            SqlTokenKind::Word => text(&token).eq_ignore_ascii_case("autocommit"),
            SqlTokenKind::Quoted => {
                let quoted = text(&token);
                let quote = quoted.chars().next().filter(|&quote| quote == '`' || (quote == '"' && lex.double_quote_is_identifier));
                quote.is_some_and(|quote| quoted.trim_matches(quote).eq_ignore_ascii_case("autocommit"))
            }
            _ => false,
        };
        let user_variable = at_signs == 1;
        at_signs = if is_other(&token, "@") { at_signs + 1 } else { 0 };
        if !named || user_variable {
            continue;
        }
        let _ = tokens.next_if(|token| is_other(token, ":"));
        if tokens.next_if(|token| is_other(token, "=")).is_none() {
            continue;
        }
        let Some(value) = tokens.next() else { break };
        let value = text(&value);
        if ["0", "off", "false"].iter().any(|off| value.eq_ignore_ascii_case(off)) {
            return Some(false);
        }
        if ["1", "on", "true"].iter().any(|on| value.eq_ignore_ascii_case(on)) {
            return Some(true);
        }
    }
    None
}

/// How a statement sent on a tab's session ended, for [`TabTx::finished`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Done,
    /// The server refused it.
    Failed,
    /// It never ran (the driver asked the user first, or refused it for a read-only connection).
    NotRun,
}

/// How many statements a tab's transaction log keeps (the oldest go first).
pub const LOG_MAX_ENTRIES: usize = 200;
/// Longest statement text a log entry keeps, in characters.
pub const LOG_TEXT_CHARS: usize = 200;
/// How much of a statement is read for the log: the rest of a long script is never looked at.
const LOG_SCAN_BYTES: usize = 2048;

/// A word whose literal that follows is a secret: `PASSWORD 'x'`, `IDENTIFIED BY 'x'`,
/// `password = 'x'`, `MASTER_PASSWORD='x'`, `api_secret := 'x'`.
fn is_secret_word(word: &str) -> bool {
    word.eq_ignore_ascii_case("by") || contains_secret_word(word)
}

/// A name that stands for a secret: it contains `password`, `passwd`, `secret` or `token`, or is
/// exactly `pw`, `pwd`, `passphrase`, `api_key`, `apikey` or `secret_key` (not any word with `key`).
fn contains_secret_word(word: &str) -> bool {
    let lower = word.to_ascii_lowercase();
    ["password", "passwd", "secret", "token"].iter().any(|part| lower.contains(part))
        || ["pw", "pwd", "passphrase", "api_key", "apikey", "secret_key"].contains(&lower.as_str())
}

/// `N'x'`, `X'x'`, `B'x'`, `_utf8mb4'x'`, `U&'x'`: what stands before a literal and belongs to it.
fn is_introducer(word: &str) -> bool {
    matches!(word.to_ascii_lowercase().as_str(), "n" | "x" | "b" | "u" | "e") || word.starts_with('_')
}

/// A string that carries a secret of its own: a connection string (`host=h password=pw`, an option
/// list) or a URI with credentials (`postgres://u:pw@h/db`).
fn literal_has_secret(literal: &str) -> bool {
    let lower = literal.to_ascii_lowercase();
    for key in ["password", "passwd", "pwd", "secret", "token"] {
        let mut from = 0;
        while let Some(at) = lower[from..].find(key) {
            let after = &lower[from + at + key.len()..];
            if after.trim_start_matches([' ', '"', '\'', '`']).starts_with('=') {
                return true;
            }
            from += at + key.len();
        }
    }
    lower.find("://").is_some_and(|at| {
        let rest = &lower[at + 3..];
        let authority = &rest[..rest.find(['/', ' ', '?']).unwrap_or(rest.len())];
        authority.split_once('@').is_some_and(|(userinfo, _)| userinfo.contains(':'))
    })
}

/// A statement as the transaction log shows it, see [`log_text`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogText {
    /// One line, secrets hidden, cut.
    pub line: String,
    /// Something was hidden: the server's echo of the statement is not kept either.
    pub hidden: bool,
    /// What was hidden (with and without its quotes), to wipe from what the server says back.
    pub secrets: Vec<String>,
}

/// `sql` as the transaction log shows it: the first [`LOG_SCAN_BYTES`] bytes, every secret hidden,
/// on one line, cut at [`LOG_TEXT_CHARS`].
///
/// A literal is hidden when it follows a password word (`PASSWORD`, `IDENTIFIED BY ... REPLACE`,
/// `IDENTIFIED WITH x AS`, anything named like a secret), also through a call (`crypt('pw',
/// gen_salt('bf'))`, `md5('pw')`) and a quoted name (`"password" = 'x'`); when the column list
/// of an `INSERT` names a secret, every literal after it; and when it holds a secret of its own (a
/// connection string, a URI with credentials). Then the line goes through [`redact_secrets`].
pub fn log_text(sql: &str, lex: &SqlLexOptions) -> LogText {
    let mut end = sql.len().min(LOG_SCAN_BYTES);
    while !sql.is_char_boundary(end) {
        end -= 1;
    }
    let head = &sql[..end];
    let scan = tokens(head, lex);
    let toks = &scan.tokens;
    let text = |index: usize| &head[toks[index].range.clone()];
    let mut hide = vec![false; toks.len()];
    // What the current statement said so far.
    let (mut identified, mut insert, mut columns_done) = (false, false, false);
    for index in 0..toks.len() {
        match toks[index].kind {
            SqlTokenKind::Semicolon => (identified, insert, columns_done) = (false, false, false),
            SqlTokenKind::Word => {
                let word = text(index);
                identified |= word.eq_ignore_ascii_case("identified");
                insert |= word.eq_ignore_ascii_case("insert") || word.eq_ignore_ascii_case("replace");
                if is_secret_word(word) || (identified && (word.eq_ignore_ascii_case("as") || word.eq_ignore_ascii_case("using"))) {
                    hide_after(toks, index, head, &mut hide);
                }
            }
            SqlTokenKind::Open if insert && !columns_done => {
                // The column list of an INSERT: a secret column hides every literal after the list.
                columns_done = true;
                let mut depth = 0;
                let close = (index..toks.len()).find(|&at| {
                    match toks[at].kind {
                        SqlTokenKind::Open => depth += 1,
                        SqlTokenKind::Close => depth -= 1,
                        _ => {}
                    }
                    depth == 0
                });
                if let Some(close) = close {
                    let secret_column = (index..close).any(|at| matches!(toks[at].kind, SqlTokenKind::Word | SqlTokenKind::Quoted) && contains_secret_word(text(at)));
                    if secret_column {
                        for later in close + 1..toks.len() {
                            match toks[later].kind {
                                SqlTokenKind::Semicolon => break,
                                SqlTokenKind::Quoted => hide[later] = true,
                                _ => {}
                            }
                        }
                    }
                }
            }
            SqlTokenKind::Quoted => {
                let literal = text(index);
                let name = literal.trim_matches(['"', '`', '[', ']']);
                if literal.starts_with(['"', '`', '[']) && contains_secret_word(name) {
                    // A quoted name: `"password" = 'x'`.
                    hide_after(toks, index, head, &mut hide);
                } else if literal_has_secret(literal) {
                    hide[index] = true;
                }
            }
            _ => {}
        }
    }
    let mut shown = String::with_capacity(head.len().min(LOG_TEXT_CHARS * 2));
    let (mut copied, mut secrets) = (0, Vec::new());
    for token in toks.iter().enumerate().filter(|(index, _)| hide[*index]).map(|(_, token)| token) {
        shown.push_str(&head[copied..token.range.start]);
        shown.push_str(REDACTED_LITERAL);
        copied = token.range.end;
        let literal = &head[token.range.clone()];
        secrets.push(literal.to_owned());
        if let Some(inner) = literal.get(1..literal.len().saturating_sub(1)).filter(|inner| inner.len() >= 3) {
            secrets.push(inner.to_owned());
        }
    }
    shown.push_str(&head[copied..]);
    let mut line = String::with_capacity(LOG_TEXT_CHARS + 3);
    for (index, word) in shown.split_whitespace().enumerate() {
        if index > 0 {
            line.push(' ');
        }
        line.push_str(word);
        if line.len() > LOG_TEXT_CHARS * 4 {
            break;
        }
    }
    let redacted = crate::sessions::redact_secrets(&line);
    let hidden = !secrets.is_empty() || redacted != line;
    let mut line = redacted;
    if line.chars().count() > LOG_TEXT_CHARS || end < sql.len() {
        line = line.chars().take(LOG_TEXT_CHARS).collect();
        line.push('\u{2026}');
    }
    LogText { line, hidden, secrets }
}

/// Marks the literals that a secret word at `at` stands for: `= 'x'`, `'x' REPLACE 'old'`, and every
/// literal inside the call that follows (`crypt('pw', gen_salt('bf'))`). Stops at the next plain word,
/// `,` or `;`.
fn hide_after(toks: &[SqlToken], at: usize, head: &str, hide: &mut [bool]) {
    let mut depth = 0_i32;
    let mut index = at + 1;
    while index < toks.len() {
        match toks[index].kind {
            SqlTokenKind::Semicolon => break,
            SqlTokenKind::Other => {}
            SqlTokenKind::Open => depth += 1,
            SqlTokenKind::Close => {
                if depth == 0 {
                    break;
                }
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            SqlTokenKind::Comma | SqlTokenKind::Number if depth == 0 => break,
            SqlTokenKind::Comma | SqlTokenKind::Number => {}
            SqlTokenKind::Word => {
                let next = toks.get(index + 1);
                let literal_follows = next.is_some_and(|next| next.kind == SqlTokenKind::Quoted)
                    || next.is_some_and(|next| next.kind == SqlTokenKind::Other && head[next.range.clone()] == *"&")
                        && toks.get(index + 2).is_some_and(|after| after.kind == SqlTokenKind::Quoted);
                if is_introducer(&head[toks[index].range.clone()]) && literal_follows {
                    // An introducer of the literal: part of it.
                } else if depth == 0 && next.is_none_or(|next| next.kind != SqlTokenKind::Open) {
                    // A call (`crypt(`, `PASSWORD(`) is part of the value; any other word ends it.
                    break;
                }
            }
            SqlTokenKind::Quoted => {
                hide[index] = true;
                if depth == 0 {
                    // `IDENTIFIED BY 'new' REPLACE 'old'`.
                    match toks.get(index + 1) {
                        Some(next) if next.kind == SqlTokenKind::Word && head[next.range.clone()].eq_ignore_ascii_case("replace") => index += 1,
                        _ => break,
                    }
                }
            }
        }
        index += 1;
    }
}

/// How a statement of the log ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogResult {
    Ok,
    /// The server refused it (the message is in [`LogEntry::error`]).
    Failed,
    /// It was cancelled or timed out: nobody knows how far it got.
    Cancelled,
}

/// One statement sent in the transaction.
#[derive(Debug, Clone, PartialEq)]
pub struct LogEntry {
    /// When it was sent.
    pub at: SystemTime,
    pub writes: bool,
    /// The server committed on its own at this statement (MySQL / MariaDB DDL...): it is not part
    /// of what a Roll Back undoes, and neither is what came before it.
    pub commits: bool,
    /// One line, secrets hidden, cut: see [`log_text`].
    pub sql: String,
    /// Rows it returned, or affected when it returned none.
    pub rows: Option<u64>,
    pub duration: Duration,
    pub result: LogResult,
    /// The server's complaint, secrets and URIs hidden, cut.
    pub error: Option<String>,
}

/// The statement on its way.
#[derive(Debug, Clone, PartialEq)]
struct Sent {
    at: SystemTime,
    started: Instant,
    writes: bool,
    sql: String,
    /// The text carried a secret: the server's echo of it is not kept.
    hidden: bool,
    /// What was hidden, to wipe from what the server says back.
    secrets: Vec<String>,
    commits: bool,
}

/// The statements a tab sent since its last Commit / Rollback, newest last, bounded: recorded once
/// per statement (at its end), never read by a frame.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TxLog {
    entries: VecDeque<LogEntry>,
    /// Older entries pushed out by the cap.
    dropped: u32,
    sent: Option<Sent>,
}

impl TxLog {
    /// A statement is about to be sent. `commits`: MySQL / MariaDB commit before and after it by
    /// themselves (DDL, `LOCK TABLES`): what the transaction did until then cannot be rolled back.
    pub fn begin(&mut self, now: Instant, writes: bool, commits: bool, sql: &str, lex: &SqlLexOptions) {
        let LogText { line, hidden, secrets } = log_text(sql, lex);
        self.sent = Some(Sent { at: SystemTime::now(), started: now, writes, sql: line, hidden, secrets, commits });
    }

    /// The statement on its way ended. `error`: the server's message.
    pub fn finish(&mut self, result: LogResult, rows: Option<u64>, error: Option<&str>, now: Instant) {
        let Some(sent) = self.sent.take() else { return };
        let error = error.map(|message| {
            let message = if sent.hidden { Scrubber::without_echo(message) } else { message.to_owned() };
            let mut scrubber = Scrubber::default();
            sent.secrets.iter().for_each(|secret| scrubber.hide(secret));
            let message = scrubber.scrub(&message);
            let mut line: String = message.split_whitespace().collect::<Vec<_>>().join(" ");
            if line.chars().count() > LOG_TEXT_CHARS {
                line = line.chars().take(LOG_TEXT_CHARS).collect();
                line.push('\u{2026}');
            }
            line
        });
        if self.entries.len() == LOG_MAX_ENTRIES {
            self.entries.pop_front();
            self.dropped = self.dropped.saturating_add(1);
        }
        self.entries.push_back(LogEntry {
            at: sent.at,
            writes: sent.writes,
            sql: sent.sql,
            commits: sent.commits,
            rows,
            duration: now.saturating_duration_since(sent.started),
            result,
            error,
        });
    }

    /// The statement on its way never ran (the driver asked first, or refused it).
    pub fn forget_sent(&mut self) {
        self.sent = None;
    }

    /// The newest entry, taken out.
    pub fn take_last(&mut self) -> Option<LogEntry> {
        self.entries.pop_back()
    }

    /// An entry given back after the log was cleared.
    pub fn restore(&mut self, entry: LogEntry) {
        self.entries.push_back(entry);
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.dropped = 0;
        self.sent = None;
    }

    pub fn entries(&self) -> impl ExactSizeIterator<Item = &LogEntry> {
        self.entries.iter()
    }

    /// How many older statements the cap pushed out.
    pub fn dropped(&self) -> u32 {
        self.dropped
    }
}

/// The commit mode of a tab and what the app knows of the transaction its session has open.
#[derive(Debug, Clone, PartialEq)]
pub struct TabTx {
    pub mode: CommitMode,
    /// Statements sent on the tab's session since the last Commit / Rollback, the failed ones
    /// included. The server decides what that left open; this only says there is something to ask
    /// about.
    pub statements: u32,
    /// Of those, the ones that modify: not a pure read, or one that failed (an error is counted
    /// as modifying, as the transaction may have changed anyway).
    pub modifying: u32,
    /// When the first of them was sent.
    pub since: Option<Instant>,
    /// When the last one was sent or ended: a transaction idle for too long is rolled back.
    pub last_active: Option<Instant>,
    /// PostgreSQL: the last statement failed, which aborts the transaction until a rollback.
    pub aborted_hint: bool,
    /// The tab's session was lost with work on it: what to tell the user until the next statement.
    pub lost: Option<String>,
    /// A counted statement is on its way: a cancel or a timeout then lands on it.
    running: bool,
    /// ... and it was a write (so a cancel does not count it a second time).
    last_writes: bool,
    /// Smart commit switched this tab to manual: a Commit / Roll Back may switch it back.
    pub smart: bool,
    /// ... and the statement that switched it has not ended yet (if it fails, the switch is undone).
    pub smart_fresh: bool,
    /// The statements of the transaction.
    pub log: TxLog,
    /// The tab runs on a session of its own while in auto-commit (smart commit's: autocommit on, no
    /// transaction until a statement modifies). Never moved to another session.
    pub auto_own: bool,
    /// MySQL / MariaDB: the next statement starts with `SET autocommit = 0` (smart commit started the
    /// transaction on a session that has autocommit on).
    pub autocommit_pending: bool,
    /// MySQL / MariaDB: smart commit returned the tab to auto-commit but its session still has
    /// autocommit off: the next request on it starts with `SET autocommit = 1`, in order with it.
    pub autocommit_restore: bool,
}

impl TabTx {
    pub fn new(mode: CommitMode) -> Self {
        Self {
            mode,
            statements: 0,
            modifying: 0,
            since: None,
            last_active: None,
            aborted_hint: false,
            lost: None,
            running: false,
            last_writes: false,
            smart: false,
            smart_fresh: false,
            log: TxLog::default(),
            auto_own: false,
            autocommit_pending: false,
            autocommit_restore: false,
        }
    }

    /// Something was sent since the last Commit / Rollback: the buttons act.
    pub fn is_open(&self) -> bool {
        self.statements > 0
    }

    /// Changes may be lost with the session: closing it is asked about.
    pub fn has_changes(&self) -> bool {
        self.modifying > 0
    }

    /// A statement is about to be sent on the tab's session. `writes`: it is not a pure read.
    pub fn sending(&mut self, now: Instant, writes: bool) {
        self.lost = None;
        self.statements = self.statements.saturating_add(1);
        self.modifying = self.modifying.saturating_add(u32::from(writes));
        self.since.get_or_insert(now);
        self.last_active = Some(now);
        self.running = true;
        self.last_writes = writes;
    }

    /// The statement on its way was cancelled or timed out. Nobody knows how far it got: it counts
    /// as modifying, and PostgreSQL has aborted the transaction around it.
    pub fn cancelled(&mut self, postgres: bool) {
        if !std::mem::take(&mut self.running) {
            return;
        }
        self.modifying = self.modifying.saturating_add(u32::from(!self.last_writes));
        self.aborted_hint = postgres;
        self.log.finish(LogResult::Cancelled, None, None, Instant::now());
    }

    /// The statement sent last ended. `ends`: it was a typed `COMMIT` / `ROLLBACK` (the server
    /// ended the transaction, or had none). `postgres`: a failure aborts the transaction.
    pub fn finished(&mut self, outcome: Outcome, writes: bool, ends: bool, postgres: bool, now: Instant) {
        self.running = false;
        match outcome {
            Outcome::NotRun => {
                self.log.forget_sent();
                self.statements = self.statements.saturating_sub(1);
                self.modifying = self.modifying.saturating_sub(u32::from(writes));
                if self.statements == 0 {
                    self.ended();
                }
                return;
            }
            Outcome::Failed => {
                self.aborted_hint = postgres;
                self.modifying = self.modifying.saturating_add(u32::from(!writes));
            }
            Outcome::Done => self.aborted_hint = false,
        }
        self.last_active = Some(now);
        if ends {
            self.ended();
        }
    }

    /// Commit or Rollback ran (or the server ended the transaction another way).
    pub fn ended(&mut self) {
        self.statements = 0;
        self.modifying = 0;
        self.since = None;
        self.last_active = None;
        self.aborted_hint = false;
        self.running = false;
        self.log.clear();
    }

    /// A `COMMIT` / `ROLLBACK AND CHAIN` ran: the transaction ended and the next one is open already,
    /// holding nothing but that statement.
    pub fn chained(&mut self, now: Instant) {
        self.ended();
        self.statements = 1;
        self.since = Some(now);
        self.last_active = Some(now);
    }

    /// MySQL / MariaDB committed by themselves at the statement that ended last (DDL, `LOCK TABLES`...):
    /// what the transaction held until then is on the server. The counters start again, and the
    /// log keeps that statement as the boundary.
    pub fn server_committed(&mut self) {
        let boundary = self.log.take_last();
        self.ended();
        if let Some(entry) = boundary {
            self.log.restore(entry);
        }
    }

    /// The session was dropped with work on it.
    pub fn lost(&mut self, reason: impl Into<String>) {
        self.ended();
        self.lost = Some(reason.into());
    }

    /// Changes sit in the transaction and nothing was sent for `limit`.
    pub fn idle_beyond(&self, limit: Duration, now: Instant) -> bool {
        self.has_changes() && self.last_active.is_some_and(|last| now.saturating_duration_since(last) >= limit)
    }
}

/// `5s`, `2m 05s`, `1h 02m`.
pub fn elapsed_label(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3599 => format!("{}m {:02}s", seconds / 60, seconds % 60),
        _ => format!("{}h {:02}m", seconds / 3600, seconds % 3600 / 60),
    }
}

/// `1 statement`, `3 statements`.
pub fn statements_label(count: u32) -> String {
    match count {
        1 => "1 statement".to_owned(),
        count => format!("{count} statements"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(sql: &str) -> Typed {
        Typed::of(sql, &SqlLexOptions::postgres())
    }

    #[test]
    fn only_postgres_and_sqlite_get_a_begin_and_only_the_mysql_family_turns_autocommit_off() {
        assert_eq!(begin_statement(ConnectionKind::Postgres), Some("BEGIN"));
        assert_eq!(begin_statement(ConnectionKind::Sqlite), Some("BEGIN"));
        for kind in [ConnectionKind::Mysql, ConnectionKind::Mariadb, ConnectionKind::Clickhouse, ConnectionKind::Redis, ConnectionKind::Mongodb] {
            assert_eq!(begin_statement(kind), None, "{kind:?}");
        }
        for kind in [ConnectionKind::Mysql, ConnectionKind::Mariadb] {
            assert_eq!(session_setup(kind), Some("SET autocommit = 0"), "{kind:?}");
        }
        for kind in [ConnectionKind::Postgres, ConnectionKind::Sqlite] {
            assert_eq!(session_setup(kind), None, "{kind:?}");
        }
        for kind in [ConnectionKind::Clickhouse, ConnectionKind::Redis, ConnectionKind::Mongodb] {
            assert!(manual_unavailable(kind, false, false).is_some(), "{kind:?}");
        }
    }

    #[test]
    fn manual_is_unavailable_on_a_read_only_connection_and_an_in_memory_sqlite() {
        for kind in [ConnectionKind::Postgres, ConnectionKind::Mysql, ConnectionKind::Mariadb, ConnectionKind::Sqlite] {
            assert_eq!(manual_unavailable(kind, false, false), None, "{kind:?}");
            assert!(manual_unavailable(kind, true, false).is_some_and(|reason| reason.contains("read-only")), "{kind:?}");
        }
        assert!(manual_unavailable(ConnectionKind::Sqlite, false, true).is_some_and(|reason| reason.contains("in-memory")));
        assert_eq!(manual_unavailable(ConnectionKind::Postgres, false, true), None, "only SQLite is in memory");
    }

    #[test]
    fn the_begin_goes_before_every_statement_but_the_whole_transaction_statements() {
        for sql in ["BEGIN", "start transaction", "BEGIN ISOLATION LEVEL SERIALIZABLE"] {
            assert_eq!(typed(sql), Typed::Begin, "{sql}");
        }
        for sql in ["COMMIT", "ROLLBACK", "END", "ABORT", "commit work", "  -- done\n COMMIT"] {
            assert_eq!(typed(sql), Typed::End, "{sql}");
        }
        for sql in ["COMMIT PREPARED 'a'", "rollback prepared 'a'"] {
            assert_eq!(typed(sql), Typed::Prepared, "{sql}");
        }
        assert_eq!(typed("PREPARE TRANSACTION 'a'"), Typed::PrepareTransaction);
        assert_eq!(typed("savepoint a"), Typed::Savepoint);
        for sql in [
            "SELECT 1",
            "INSERT INTO t VALUES (1)",
            "ROLLBACK TO SAVEPOINT a",
            "RELEASE SAVEPOINT a",
            "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE",
            "SET \"application_name\" = 'x'",
            "LOCK TABLE t",
            "PREPARE p AS SELECT 1",
            "   -- nothing\n",
        ] {
            assert_eq!(typed(sql), Typed::Other, "{sql}");
            assert!(typed(sql).takes_begin(), "{sql}");
        }
        for sql in ["BEGIN", "COMMIT", "COMMIT PREPARED 'a'", "PREPARE TRANSACTION 'a'"] {
            assert!(!typed(sql).takes_begin(), "{sql}");
        }
        assert!(typed("SAVEPOINT a").takes_begin());
    }

    #[test]
    fn only_the_first_statement_is_read_so_a_trigger_body_is_not_a_transaction_statement() {
        let trigger = "CREATE TRIGGER t AFTER INSERT ON a BEGIN UPDATE b SET x = 1; END";
        assert_eq!(Typed::of(trigger, &SqlLexOptions::sqlite()), Typed::Other);
    }

    fn run(tx: &mut TabTx, outcome: Outcome, writes: bool, ends: bool, postgres: bool) {
        let now = Instant::now();
        tx.sending(now, writes);
        tx.finished(outcome, writes, ends, postgres, now);
    }

    #[test]
    fn an_empty_database_is_the_connections_default_one() {
        let pg = ConnectionKind::Postgres;
        let uri = "postgres://u:p@host:5432/shop?sslmode=require";
        assert!(same_database(pg, uri, "", "shop") && same_database(pg, uri, "shop", ""));
        assert!(!same_database(pg, uri, "", "other"));
        assert!(same_database(pg, "postgres://host", "", ""));
        assert!(same_database(ConnectionKind::Sqlite, "sqlite:///a.db", "", "main"));
        assert!(!same_database(ConnectionKind::Sqlite, "sqlite:///a.db", "", "temp"));
        assert!(mentions_autocommit("set @@AutoCommit = 0") && !mentions_autocommit("SELECT 1"));
    }

    #[test]
    fn autocommit_settings_are_read_for_both_values() {
        for (sql, on) in [("SET autocommit = 0", false), ("set @@autocommit=OFF", false), ("SET SESSION autocommit := false", false), ("SET autocommit=1", true), ("set autocommit = ON", true)] {
            assert_eq!(Typed::of(sql, &SqlLexOptions::mysql()), Typed::Autocommit(on), "{sql}");
        }
        assert_eq!(Typed::of("SET names utf8", &SqlLexOptions::mysql()), Typed::Other);
    }

    /// The app read the text after the keyword with its own scan (the first `autocommit` found, wherever
    /// it stood), the driver with the lexer: these are the inputs the two read differently.
    #[test]
    fn autocommit_is_read_the_way_the_session_lexer_reads_it_not_by_a_text_search() {
        let mysql = SqlLexOptions::mysql();
        // Text that only spells it: a string, comments (`/* */`, `#`, `-- `), a user variable.
        for sql in [
            "SET @note = 'autocommit = 0'",
            "SET /* autocommit = 0 */ @a = 1",
            "SET @a = 1 # autocommit = 0",
            "SET @a = 1 -- autocommit = 0",
            "SET @my_autocommit = 0",
            "SET @autocommit = 0",
            "SET @a = 1, @b = 'x autocommit = 1'",
        ] {
            assert_eq!(Typed::of(sql, &mysql), Typed::Other, "{sql}");
        }
        // The setting itself, after text that only spells it: the first occurrence is not the one that counts.
        for (sql, on) in [
            ("SET @a = 'autocommit', autocommit = 1", true),
            ("SET /* autocommit */ autocommit = 0", false),
            ("SET @a = 1 /* autocommit = 1 */, @@session.autocommit = 0", false),
            ("SET @@autocommit = 1, @x = 2", true),
            ("SET `autocommit` = 0", false),
        ] {
            assert_eq!(Typed::of(sql, &mysql), Typed::Autocommit(on), "{sql}");
        }
        // NO_BACKSLASH_ESCAPES: `'\\'` is a whole string, so the assignment after it is real; with the
        // default it never ends and nothing in it is.
        let no_backslash = "SET @s = '\\', autocommit = 0";
        assert_eq!(Typed::of(no_backslash, &mysql.without_backslash_escapes()), Typed::Autocommit(false));
        assert_eq!(Typed::of(no_backslash, &mysql), Typed::Other);
        // ANSI_QUOTES: `"autocommit"` is a name only then.
        assert_eq!(Typed::of("SET \"autocommit\" = 0", &mysql.with_ansi_quotes()), Typed::Autocommit(false));
        assert_eq!(Typed::of("SET \"autocommit\" = 0", &mysql), Typed::Other);
    }

    /// `AND CHAIN` starts the next transaction at once; `AND NO CHAIN` and a comment that says so do not.
    #[test]
    fn a_commit_or_rollback_and_chain_is_not_the_end_of_the_transaction() {
        for lex in [SqlLexOptions::postgres(), SqlLexOptions::mysql()] {
            for sql in ["COMMIT AND CHAIN", "rollback work and chain", "COMMIT /* x */ AND\n CHAIN", "ROLLBACK TRANSACTION AND CHAIN"] {
                assert_eq!(Typed::of(sql, &lex), Typed::Chain, "{sql}");
                assert!(!Typed::of(sql, &lex).takes_begin(), "{sql}");
            }
            for sql in ["COMMIT AND NO CHAIN", "COMMIT", "COMMIT -- and chain", "COMMIT /* AND CHAIN */", "END"] {
                assert_eq!(Typed::of(sql, &lex), Typed::End, "{sql}");
            }
        }
        let mut tx = TabTx::new(CommitMode::Manual);
        let now = Instant::now();
        run(&mut tx, Outcome::Done, true, false, true);
        run(&mut tx, Outcome::Done, true, false, true);
        tx.chained(now);
        assert_eq!((tx.statements, tx.modifying, tx.is_open()), (1, 0, true), "the next transaction is open, holding the chain statement");
    }

    #[test]
    fn prepared_transactions_are_told_by_words_not_by_the_text_after_the_keyword() {
        let pg = SqlLexOptions::postgres();
        // PostgreSQL nests block comments: the comment ends at the last `*/`, and the word after it counts.
        assert_eq!(Typed::of("COMMIT /* a /* b */ c */ PREPARED 'x'", &pg), Typed::Prepared);
        assert_eq!(Typed::of("PREPARE -- the id\n TRANSACTION 'x'", &pg), Typed::PrepareTransaction);
        assert_eq!(Typed::of("PREPARE TRANSACTION $$x$$", &pg), Typed::PrepareTransaction);
        // Not a word of its own.
        assert_eq!(Typed::of("COMMIT 'prepared'", &pg), Typed::End);
        assert_eq!(Typed::of("ROLLBACK TO preparedpoint", &pg), Typed::Other);
    }

    #[test]
    fn the_counters_count_every_statement_sent_and_the_modifying_ones_apart() {
        let mut tx = TabTx::new(CommitMode::Manual);
        assert!(!tx.is_open() && !tx.has_changes());
        let now = Instant::now();
        tx.sending(now, false);
        tx.finished(Outcome::Done, false, false, false, now);
        assert!(tx.is_open() && !tx.has_changes(), "reads only: something to end, nothing to lose");
        tx.sending(now + Duration::from_secs(5), true);
        tx.finished(Outcome::Done, true, false, false, now);
        run(&mut tx, Outcome::Failed, false, false, false);
        assert_eq!((tx.statements, tx.modifying), (3, 2), "a failed read counts as modifying");
        assert_eq!(tx.since, Some(now), "from the first statement");
    }

    #[test]
    fn a_typed_end_resets_the_counters_whatever_the_outcome() {
        for outcome in [Outcome::Done, Outcome::Failed] {
            let mut tx = TabTx::new(CommitMode::Manual);
            run(&mut tx, Outcome::Done, true, false, true);
            run(&mut tx, outcome, true, true, true);
            assert_eq!((tx.statements, tx.modifying, tx.since, tx.aborted_hint), (0, 0, None, false), "{outcome:?}");
        }
    }

    #[test]
    fn a_statement_that_never_ran_is_not_counted() {
        let mut tx = TabTx::new(CommitMode::Manual);
        run(&mut tx, Outcome::NotRun, true, false, true);
        assert_eq!((tx.statements, tx.modifying, tx.since), (0, 0, None));
        run(&mut tx, Outcome::Done, true, false, false);
        run(&mut tx, Outcome::NotRun, true, true, false);
        assert_eq!((tx.statements, tx.modifying), (1, 1), "and a COMMIT held back ended nothing");
    }

    #[test]
    fn the_postgres_hint_follows_the_last_statement() {
        let mut tx = TabTx::new(CommitMode::Manual);
        run(&mut tx, Outcome::Failed, true, false, true);
        assert!(tx.aborted_hint);
        run(&mut tx, Outcome::Done, false, false, true);
        assert!(!tx.aborted_hint, "cleared by the next statement that ran");
        run(&mut tx, Outcome::Failed, true, false, false);
        assert!(!tx.aborted_hint, "only PostgreSQL aborts");
        run(&mut tx, Outcome::Failed, true, false, true);
        tx.ended();
        assert!(!tx.aborted_hint, "and by a rollback");
    }

    #[test]
    fn a_transaction_with_changes_is_idle_after_the_limit_and_reads_are_not() {
        let start = Instant::now();
        let mut tx = TabTx::new(CommitMode::Manual);
        tx.sending(start, false);
        tx.finished(Outcome::Done, false, false, false, start);
        let limit = Duration::from_secs(60);
        assert!(!tx.idle_beyond(limit, start + Duration::from_secs(600)), "only reads: closed silently, never rolled back by the timer");
        tx.sending(start, true);
        tx.finished(Outcome::Done, true, false, false, start);
        assert!(!tx.idle_beyond(limit, start + Duration::from_secs(59)));
        assert!(tx.idle_beyond(limit, start + Duration::from_secs(60)));
        tx.ended();
        assert!(!tx.idle_beyond(limit, start + Duration::from_secs(600)));
    }

    #[test]
    fn a_cancelled_statement_counts_as_modifying_once_and_aborts_a_postgres_transaction() {
        let mut tx = TabTx::new(CommitMode::Manual);
        tx.sending(Instant::now(), false);
        tx.cancelled(true);
        assert_eq!((tx.statements, tx.modifying, tx.aborted_hint), (1, 1, true));
        tx.cancelled(true);
        assert_eq!(tx.modifying, 1, "nothing was on its way the second time");
        // A write was counted when it was sent: not twice.
        let mut tx = TabTx::new(CommitMode::Manual);
        tx.sending(Instant::now(), true);
        tx.cancelled(false);
        assert_eq!((tx.modifying, tx.aborted_hint), (1, false));
        // One that finished is not on its way any more.
        let mut tx = TabTx::new(CommitMode::Manual);
        run(&mut tx, Outcome::Done, false, false, true);
        tx.cancelled(true);
        assert_eq!((tx.modifying, tx.aborted_hint), (0, false));
    }

    #[test]
    fn a_lost_session_resets_the_counters_and_says_why_until_the_next_statement() {
        let mut tx = TabTx::new(CommitMode::Manual);
        tx.sending(Instant::now(), true);
        tx.lost("the connection dropped");
        assert_eq!((tx.statements, tx.modifying, tx.lost.as_deref()), (0, 0, Some("the connection dropped")));
        tx.sending(Instant::now(), false);
        assert_eq!(tx.lost, None);
    }

    fn shown(sql: &str) -> LogText {
        log_text(sql, &SqlLexOptions::mysql())
    }

    fn shown_pg(sql: &str) -> LogText {
        log_text(sql, &SqlLexOptions::postgres())
    }

    #[test]
    fn the_log_text_hides_the_literal_after_a_password_word_and_nothing_else() {
        for (sql, expected) in [
            ("CREATE USER bob IDENTIFIED BY 'hunter2'", "CREATE USER bob IDENTIFIED BY '********'"),
            ("ALTER ROLE bob WITH LOGIN PASSWORD 'hunter2' VALID UNTIL 'infinity'", "ALTER ROLE bob WITH LOGIN PASSWORD '********' VALID UNTIL 'infinity'"),
            ("UPDATE users SET password = 'hunter2' WHERE name = 'bob'", "UPDATE users SET password = '********' WHERE name = 'bob'"),
            ("SET PASSWORD FOR bob = PASSWORD('hunter2')", "SET PASSWORD FOR bob = PASSWORD('********')"),
            ("CHANGE MASTER TO MASTER_PASSWORD='hunter2'", "CHANGE MASTER TO MASTER_PASSWORD='********'"),
            ("CREATE USER b IDENTIFIED WITH caching_sha2_password BY \"hunter2\"", "CREATE USER b IDENTIFIED WITH caching_sha2_password BY '********'"),
        ] {
            let text = shown(sql);
            assert_eq!((text.line.as_str(), text.hidden), (expected, true), "{sql}");
            assert!(!text.line.contains("hunter2"), "{sql}");
        }
        let text = shown("SELECT *\n  FROM t\n WHERE name = 'bob'");
        assert_eq!((text.line.as_str(), text.hidden), ("SELECT * FROM t WHERE name = 'bob'", false), "one line, no secret");
        let text = shown("SELECT password FROM users WHERE id = 7 ORDER BY name");
        assert!(!text.hidden, "a column named like a secret is not a secret: {}", text.line);
    }

    #[test]
    fn secrets_in_calls_quoted_names_replacements_hashes_and_literals_of_their_own_are_hidden_too() {
        for (sql, kind) in [
            ("UPDATE u SET password = crypt('hunter2', gen_salt('bf')) WHERE id = 1", "pg"),
            ("UPDATE u SET password = md5('hunter2')", "pg"),
            ("UPDATE u SET \"password\" = 'hunter2'", "pg"),
            ("UPDATE u SET `password` = 'hunter2'", "my"),
            ("ALTER USER bob IDENTIFIED BY 'hunter2' REPLACE 'hunter1'", "my"),
            ("CREATE USER bob IDENTIFIED WITH mysql_native_password AS 'hunter2'", "my"),
            ("CREATE USER bob IDENTIFIED WITH auth_plugin USING 'hunter2'", "my"),
            ("CREATE SUBSCRIPTION s CONNECTION 'host=h port=5432 dbname=d password=hunter2' PUBLICATION p", "pg"),
            ("SELECT dblink_connect('myconn', 'dbname=d user=u password=hunter2')", "pg"),
            ("CREATE SERVER s FOREIGN DATA WRAPPER postgres_fdw OPTIONS (host 'h', password 'hunter2')", "pg"),
            ("SELECT * FROM x WHERE u = 'postgres://bob:hunter2@db.example.com:5432/app'", "pg"),
            ("INSERT INTO users(name, password) VALUES ('bob', 'hunter2')", "pg"),
            ("INSERT INTO users (id, \"password\") VALUES (1, 'hunter2'), (2, 'hunter2')", "pg"),
            ("INSERT INTO users(name, passwd) SELECT 'bob', 'hunter2'", "pg"),
            ("UPDATE u SET pw = 'hunter2'", "pg"),
            ("UPDATE u SET api_key = 'hunter2' WHERE id = 1", "pg"),
            ("ALTER USER bob IDENTIFIED BY N'hunter2'", "my"),
            ("ALTER USER bob IDENTIFIED BY _utf8mb4'hunter2'", "my"),
            ("UPDATE u SET password = U&'hunter2'", "pg"),
        ] {
            let text = if kind == "pg" { shown_pg(sql) } else { shown(sql) };
            assert!(text.hidden && !text.line.contains("hunter2") && !text.line.contains("hunter1"), "{sql}: {}", text.line);
            assert!(text.secrets.iter().any(|secret| secret.contains("hunter2") || secret.contains("hunter1")), "{sql}");
        }
        let text = shown_pg("INSERT INTO users(name, password) VALUES ('bob', 'hunter2')");
        assert_eq!(text.line, "INSERT INTO users(name, password) VALUES ('********', '********')");
        // Not secrets.
        for sql in ["INSERT INTO users(name, email) VALUES ('bob', 'b@x.org')", "SELECT 'host=h port=5432'", "UPDATE t SET monkey = 'x', keyboard = 'y'"] {
            assert!(!shown_pg(sql).hidden, "{sql}");
        }
        // A URL is taken out whole, as everywhere the app shows driver text.
        assert!(shown_pg("SELECT 'see https://example.com/docs' AS note").line.contains("<redacted-uri>"));
    }

    #[test]
    fn what_the_server_echoes_of_a_hidden_literal_is_wiped_from_the_error() {
        // A MySQL syntax error repeats the text from the mistake on; a PostgreSQL one names the literal.
        for message in [
            "You have an error in your SQL syntax; check the manual that corresponds to your MySQL server version for the right syntax to use near ''hunter2' at line 1",
            "syntax error at or near \"hunter2\"",
            "invalid input syntax for type integer: \"hunter2\"",
            "Data too long for column 'password' at row 1: 'hunter2'",
        ] {
            let mut log = TxLog::default();
            let now = Instant::now();
            log.begin(now, true, false, "UPDATE u SET password = 'hunter2' WHERE id = 1", &SqlLexOptions::mysql());
            log.finish(LogResult::Failed, None, Some(message), now);
            let error = log.entries().next().unwrap().error.clone().unwrap();
            assert!(!error.contains("hunter2"), "{message} -> {error}");
        }
    }

    #[test]
    fn a_secret_cut_by_the_scan_limit_or_never_closed_is_still_hidden_and_long_text_is_cut() {
        let long = format!("SELECT '{}' ; CREATE USER a IDENTIFIED BY 'hunter2-{}'", "x".repeat(LOG_SCAN_BYTES - 60), "y".repeat(500));
        let text = shown(&long);
        assert!(!text.line.contains("hunter2"), "{}", text.line);
        assert!(text.line.chars().count() <= LOG_TEXT_CHARS + 1 && text.line.ends_with('\u{2026}'));
        let text = shown("CREATE USER a IDENTIFIED BY 'hunter2");
        assert!(text.hidden && !text.line.contains("hunter2"), "{}", text.line);
        let text = shown(&format!("SELECT {}", "\u{00E9}".repeat(LOG_SCAN_BYTES)));
        assert!(text.line.chars().count() <= LOG_TEXT_CHARS + 1);
    }

    fn logged(tx: &mut TabTx, sql: &str, writes: bool, result: LogResult) {
        let now = Instant::now();
        tx.sending(now, writes);
        tx.log.begin(now, writes, false, sql, &SqlLexOptions::postgres());
        tx.log.finish(result, Some(1), (result == LogResult::Failed).then_some("boom"), now + Duration::from_millis(5));
        tx.finished(if result == LogResult::Failed { Outcome::Failed } else { Outcome::Done }, writes, false, false, now);
    }

    #[test]
    fn the_log_keeps_each_statement_once_in_order_and_is_cleared_when_the_transaction_ends() {
        let mut tx = TabTx::new(CommitMode::Manual);
        logged(&mut tx, "SELECT 1", false, LogResult::Ok);
        logged(&mut tx, "UPDATE t SET a = 1", true, LogResult::Ok);
        logged(&mut tx, "INSERT INTO nope VALUES (1)", true, LogResult::Failed);
        let entries: Vec<_> = tx.log.entries().collect();
        assert_eq!(entries.iter().map(|entry| (entry.sql.as_str(), entry.writes, entry.result)).collect::<Vec<_>>(), [
            ("SELECT 1", false, LogResult::Ok),
            ("UPDATE t SET a = 1", true, LogResult::Ok),
            ("INSERT INTO nope VALUES (1)", true, LogResult::Failed),
        ]);
        assert_eq!((entries[2].error.as_deref(), entries[0].rows, entries[0].duration), (Some("boom"), Some(1), Duration::from_millis(5)));
        tx.ended();
        assert_eq!(tx.log.entries().len(), 0);
        assert_eq!(tx.log.dropped(), 0);
    }

    #[test]
    fn the_log_is_capped_and_says_how_many_it_pushed_out() {
        let mut tx = TabTx::new(CommitMode::Manual);
        for index in 0..LOG_MAX_ENTRIES + 7 {
            logged(&mut tx, &format!("UPDATE t SET a = {index}"), true, LogResult::Ok);
        }
        assert_eq!((tx.log.entries().len(), tx.log.dropped()), (LOG_MAX_ENTRIES, 7));
        assert_eq!(tx.log.entries().next().unwrap().sql, "UPDATE t SET a = 7", "the oldest went first");
        assert_eq!(tx.statements, (LOG_MAX_ENTRIES + 7) as u32, "the counters are not capped");
    }

    #[test]
    fn a_cancelled_statement_is_logged_and_one_that_never_ran_is_not() {
        let mut tx = TabTx::new(CommitMode::Manual);
        let now = Instant::now();
        tx.sending(now, true);
        tx.log.begin(now, true, false, "DELETE FROM t", &SqlLexOptions::postgres());
        tx.cancelled(false);
        assert_eq!(tx.log.entries().map(|entry| entry.result).collect::<Vec<_>>(), [LogResult::Cancelled]);
        tx.sending(now, true);
        tx.log.begin(now, true, false, "DELETE FROM u", &SqlLexOptions::postgres());
        tx.finished(Outcome::NotRun, true, false, false, now);
        assert_eq!(tx.log.entries().len(), 1);
        tx.log.finish(LogResult::Ok, None, None, now);
        assert_eq!(tx.log.entries().len(), 1, "nothing was on its way");
    }

    #[test]
    fn the_server_echo_of_a_statement_that_carried_a_secret_is_not_kept() {
        let mut log = TxLog::default();
        let now = Instant::now();
        log.begin(now, true, false, "CREATE USER a IDENTIFIED BY 'hunter2'", &SqlLexOptions::mysql());
        log.finish(LogResult::Failed, None, Some("syntax error near 'hunter2' at line 1 (error 1064, SQLSTATE 42000)"), now);
        let error = log.entries().next().unwrap().error.clone().unwrap();
        assert!(!error.contains("hunter2") && error.contains("1064"), "{error}");
    }

    #[test]
    fn a_commit_the_server_made_by_itself_starts_the_counters_and_the_log_again_at_that_statement() {
        let mut tx = TabTx::new(CommitMode::Manual);
        logged(&mut tx, "INSERT INTO t VALUES (1)", true, LogResult::Ok);
        logged(&mut tx, "INSERT INTO t VALUES (2)", true, LogResult::Ok);
        logged(&mut tx, "CREATE INDEX i ON t (a)", true, LogResult::Ok);
        tx.server_committed();
        assert_eq!((tx.statements, tx.modifying, tx.since), (0, 0, None), "nothing is open: Roll Back has nothing to claim");
        assert_eq!(tx.log.entries().map(|entry| entry.sql.as_str()).collect::<Vec<_>>(), ["CREATE INDEX i ON t (a)"]);
        logged(&mut tx, "INSERT INTO t VALUES (3)", true, LogResult::Ok);
        assert_eq!((tx.statements, tx.log.entries().len()), (1, 2), "what comes after is a transaction of its own");
        tx.ended();
        assert_eq!(tx.log.entries().len(), 0);
    }

    #[test]
    fn labels() {
        assert_eq!(elapsed_label(Duration::from_secs(5)), "5s");
        assert_eq!(elapsed_label(Duration::from_secs(125)), "2m 05s");
        assert_eq!(elapsed_label(Duration::from_secs(3720)), "1h 02m");
        assert_eq!(statements_label(1), "1 statement");
        assert_eq!(statements_label(0), "0 statements");
        assert_eq!(statements_label(3), "3 statements");
        assert_eq!(CommitMode::default(), CommitMode::Auto);
    }
}
