//! A small, dependency-free SQL classifier for drivers.
//!
//! It lexes just enough SQL (strings, quoted identifiers, comments, PostgreSQL
//! dollar quotes, MySQL executable comments) to answer the questions the wire
//! contract in `engine_driver_api::sql` asks of every SQL driver:
//!
//! * [`Classification::pageable`]: exactly one statement that is a pure read, so
//!   the host may re-run it with a different `offset`.
//! * [`Classification::danger`]: a statement that needs the user's confirmation
//!   (`DROP`, `TRUNCATE`, `ALTER`, `GRANT`/`REVOKE`, `CREATE`/`DROP DATABASE`,
//!   `DELETE`/`UPDATE` without a top-level `WHERE`), in any statement of a script.
//! * [`Classification::changes_database`]: a `USE` anywhere in the script, also
//!   behind a comment or inside a MySQL executable comment (`/*!50000 USE x */`).
//! * [`Classification::opens_transaction`] / [`ends_transaction`](Classification::ends_transaction):
//!   hints for drivers that track a user's transaction.
//! * [`Classification::controls_transaction`]: some statement touches transaction
//!   state in any way (`BEGIN`, `COMMIT`, `ROLLBACK [TO]`, `END`, `ABORT`,
//!   `SAVEPOINT`, `RELEASE`, `SET TRANSACTION`, `SET autocommit`, `XA`,
//!   `PREPARE TRANSACTION`, `LOCK`/`UNLOCK`).
//! * [`Classification::read_only_refusal`]: the rule a driver applies before it runs
//!   text under `payload.read_only` (see [`ReadOnlyRefusal`]).
//!
//! * Versioned executable comments (`/*!50000 … */`, `/*M!100000 … */`): code to a server
//!   at or above the version, a plain comment below it. Give the server version
//!   ([`SqlLexOptions::with_server_version`]); without it (or for a form that MySQL and
//!   MariaDB read differently) the text is [`Classification::version_ambiguous`]: dangerous,
//!   never pageable, refused by a read-only run.
//!
//! The classifier is one pass over the bytes: it allocates the statement list
//! only, never a `String` per token, and never builds a parse tree. It is
//! conservative, not a parser: unknown or malformed input is simply "not a
//! pageable read".
//!
//! # Known blind spots
//!
//! The classifier is a lexer, not a parser; do not rely on it for what it cannot
//! see (the wire contract in `engine_driver_api::sql` lists the same limits):
//!
//! * Statements hidden in procedural code: `DO $$ … $$`, `PREPARE s FROM '…'` +
//!   `EXECUTE`, function and procedure bodies, `CALL`.
//! * Side effects of a function called from a pageable `SELECT` (`nextval`,
//!   user-defined functions): such a `SELECT` is still "pageable".
//! * Session settings changed inside the same script that alter how the rest of
//!   it is lexed (`standard_conforming_strings`, `sql_mode` with
//!   `NO_BACKSLASH_ESCAPES`/`ANSI_QUOTES`): the lexer options are fixed per call.
//! * Implicit commits and `ROLLBACK`s inside procedures are not tracked by the
//!   transaction hints; `SET autocommit` is only seen as a `SET` naming it (not as
//!   an `opens`/`ends` hint, but it sets `controls_transaction`).
//! * An unquoted object or column named like a keyword the classifier looks at
//!   (`analyze`, `into`) makes the statement look riskier (never safer).
//!
//! ```
//! use engine_driver_kit::sql::{classify, SqlLexOptions};
//! let pg = SqlLexOptions::postgres();
//! assert!(classify("SELECT * FROM t -- DELETE FROM t", &pg).pageable);
//! assert!(!classify("WITH d AS (DELETE FROM t RETURNING *) SELECT * FROM d", &pg).pageable);
//! assert!(classify("DELETE FROM t", &pg).danger.is_some());
//! assert!(classify("/*!50000 USE prod */", &SqlLexOptions::mysql()).changes_database);
//! ```

use std::ops::Range;

/// How the lexer reads one SQL dialect. Start from a preset and adjust.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SqlLexOptions {
    /// `\'` and `\\` escape inside `'…'` (and `"…"` strings). MySQL default.
    pub backslash_escapes: bool,
    /// `"…"` is a quoted identifier (PostgreSQL, SQLite, MySQL `ANSI_QUOTES`);
    /// otherwise it is a string literal (MySQL default).
    pub double_quote_is_identifier: bool,
    /// `` `…` `` quotes an identifier (MySQL/MariaDB, SQLite).
    pub backtick_identifiers: bool,
    /// PostgreSQL `$tag$ … $tag$` string literals.
    pub dollar_quotes: bool,
    /// PostgreSQL `E'…'` strings: backslash escapes even if
    /// `backslash_escapes` is off.
    pub e_strings: bool,
    /// `/* /* */ */` nests (PostgreSQL); otherwise the first `*/` ends it.
    pub nested_block_comments: bool,
    /// `[name]` quotes an identifier (SQLite, SQL Server); `]]` escapes `]`.
    pub bracket_identifiers: bool,
    /// `# …` line comments (MySQL/MariaDB).
    pub hash_comments: bool,
    /// MySQL requires whitespace (or end of input) after `--` to start a comment.
    pub dash_comment_needs_space: bool,
    /// `/*! … */` (and `/*M! … */` for MariaDB): the content is code.
    pub executable_comments: bool,
    /// Also treat `/*M! … */` as executable (MariaDB).
    pub mariadb_comments: bool,
    /// A backslash escapes inside `` `…` `` and `"…"` identifiers (ClickHouse).
    pub backslash_in_identifiers: bool,
    /// A carriage return ends a `--` / `#` line comment as a line feed does (PostgreSQL:
    /// its scanner's newline is `[\n\r]`). MySQL, MariaDB, SQLite and ClickHouse end it at
    /// `\n` only (measured on the test servers: `SELECT 1 -- x\r, 2` returns the `2` too
    /// on PostgreSQL only).
    pub line_comment_ends_at_cr: bool,
    /// The server's version in MySQL's numeric form (MySQL 8.4.6 = `80406`, MariaDB
    /// 11.8.9 = `110809`). A versioned executable comment (`/*!50000 … */`,
    /// `/*M!100000 … */`) is code to a server at or above its version and a plain
    /// comment below it. `None` (unknown): any versioned comment makes the text
    /// [`Classification::version_ambiguous`].
    pub server_version: Option<u32>,
}

impl SqlLexOptions {
    /// PostgreSQL with `standard_conforming_strings = on`.
    pub const fn postgres() -> Self {
        Self {
            backslash_escapes: false,
            double_quote_is_identifier: true,
            backtick_identifiers: false,
            dollar_quotes: true,
            e_strings: true,
            nested_block_comments: true,
            bracket_identifiers: false,
            hash_comments: false,
            dash_comment_needs_space: false,
            executable_comments: false,
            mariadb_comments: false,
            backslash_in_identifiers: false,
            line_comment_ends_at_cr: true,
            server_version: None,
        }
    }

    /// MySQL with the default `sql_mode` (backslash escapes, `"` is a string).
    pub const fn mysql() -> Self {
        Self {
            backslash_escapes: true,
            double_quote_is_identifier: false,
            backtick_identifiers: true,
            dollar_quotes: false,
            e_strings: false,
            nested_block_comments: false,
            bracket_identifiers: false,
            hash_comments: true,
            dash_comment_needs_space: true,
            executable_comments: true,
            mariadb_comments: false,
            backslash_in_identifiers: false,
            line_comment_ends_at_cr: false,
            server_version: None,
        }
    }

    /// MariaDB: MySQL plus `/*M! … */`.
    pub const fn mariadb() -> Self {
        let mut options = Self::mysql();
        options.mariadb_comments = true;
        options
    }

    /// SQLite: `"` identifiers, backticks accepted, `--` and `/* */` comments.
    pub const fn sqlite() -> Self {
        let mut options = Self::postgres();
        options.dollar_quotes = false;
        options.e_strings = false;
        options.nested_block_comments = false;
        options.backtick_identifiers = true;
        options.bracket_identifiers = true;
        options.line_comment_ends_at_cr = false;
        options
    }

    /// ClickHouse: `'…'` strings with backslash escapes, `"…"` and `` `…` ``
    /// identifiers (also backslash-escaped), `$tag$ … $tag$` heredocs, `--`, `#`
    /// and `/* */` comments (not nested).
    pub const fn clickhouse() -> Self {
        Self {
            backslash_escapes: true,
            double_quote_is_identifier: true,
            backtick_identifiers: true,
            dollar_quotes: true,
            e_strings: false,
            nested_block_comments: false,
            bracket_identifiers: false,
            hash_comments: true,
            dash_comment_needs_space: false,
            executable_comments: false,
            mariadb_comments: false,
            backslash_in_identifiers: true,
            line_comment_ends_at_cr: false,
            server_version: None,
        }
    }

    /// The server's version in MySQL's numeric form, so versioned executable
    /// comments are read as the server reads them.
    #[must_use]
    pub const fn with_server_version(mut self, version: u32) -> Self {
        self.server_version = Some(version);
        self
    }

    /// MySQL `sql_mode` containing `NO_BACKSLASH_ESCAPES`.
    #[must_use]
    pub const fn without_backslash_escapes(mut self) -> Self {
        self.backslash_escapes = false;
        self
    }

    /// MySQL `sql_mode` containing `ANSI_QUOTES`.
    #[must_use]
    pub const fn with_ansi_quotes(mut self) -> Self {
        self.double_quote_is_identifier = true;
        self
    }

    /// The preset adjusted to what a connection reported in its `sql_dialect`
    /// (`dbengine_connect` reply). Only the directions a session can move away
    /// from the preset: backslash escapes off (`NO_BACKSLASH_ESCAPES`), `"` an
    /// identifier (`ANSI_QUOTES`), `[name]` an identifier (MariaDB `MSSQL`).
    /// A `false` for the last two never turns the preset's own setting off: an
    /// older driver does not send them, and PostgreSQL's `"` stays an identifier.
    #[must_use]
    pub const fn with_session_dialect(mut self, info: &engine_driver_api::SqlDialectInfo) -> Self {
        if !info.string_backslash_escapes {
            self.backslash_escapes = false;
        }
        if info.double_quote_identifiers {
            self.double_quote_is_identifier = true;
        }
        if info.bracket_identifiers {
            self.bracket_identifiers = true;
        }
        self
    }
}

/// Why a statement needs confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Danger {
    /// Index into [`Classification::statements`].
    pub statement: usize,
    /// Human-readable reason, suitable as the error message.
    pub reason: &'static str,
}

/// The first keyword of a statement (after comments, whitespace and opening
/// parentheses). Words the classifier does not care about are `Other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Keyword {
    Select,
    With,
    Values,
    Table,
    Show,
    Explain,
    Describe,
    Insert,
    Update,
    Delete,
    Replace,
    Merge,
    Create,
    Drop,
    Alter,
    Truncate,
    Grant,
    Revoke,
    Use,
    Begin,
    Start,
    Commit,
    Rollback,
    End,
    Abort,
    Analyze,
    Set,
    Savepoint,
    Release,
    Prepare,
    Xa,
    Lock,
    Unlock,
    Other,
}

/// One non-empty statement of the input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement {
    /// Byte range of the statement in the input (comments around it included).
    pub range: Range<usize>,
    /// Its first keyword; `None` for a statement without any word (e.g. `(1)`).
    pub keyword: Option<Keyword>,
    /// Byte range of that first keyword, for callers that want its spelling.
    pub keyword_range: Option<Range<usize>>,
    /// A pure read (see [`Classification::pageable`]).
    pub read_only: bool,
    pub danger: Option<&'static str>,
    pub changes_database: bool,
    pub opens_transaction: bool,
    pub ends_transaction: bool,
    /// Touches transaction state in any way: everything `opens_transaction` and
    /// `ends_transaction` mean, plus `ROLLBACK TO`, `COMMIT`/`ROLLBACK PREPARED`,
    /// `SAVEPOINT`, `RELEASE`, `XA …`, `PREPARE TRANSACTION`, `LOCK`/`UNLOCK`,
    /// `START` of any kind, and a `SET` naming a transaction setting
    /// (`TRANSACTION`, `autocommit`, `transaction_read_only`, `tx_isolation`, …)
    /// or a quoted identifier (which could spell one).
    pub controls_transaction: bool,
    /// `EXPLAIN`/`DESCRIBE` of a statement, or `ANALYZE [FORMAT=…] <statement>`
    /// (MariaDB): it plans or runs the statement after it, so it is not a plain read
    /// when it analyzes, but it is the one shape a read-only run lets through besides
    /// plain reads (the server's READ ONLY scope refuses the writes it would run).
    pub wraps_statement: bool,
}

/// Result of [`classify`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Classification {
    pub statements: Vec<Statement>,
    /// Exactly one statement, and it is a pure read: `SELECT`, `VALUES`, `TABLE`,
    /// `SHOW`, `DESCRIBE`, `EXPLAIN` without `ANALYZE`, or `WITH … SELECT` whose
    /// CTEs contain no `INSERT`/`UPDATE`/`DELETE`/`MERGE`. `SELECT … INTO` is not
    /// a read (it creates a table or writes a file).
    pub pageable: bool,
    /// The first dangerous statement of the script, if any.
    pub danger: Option<Danger>,
    /// Some statement is `USE <database>`.
    pub changes_database: bool,
    /// Some statement starts a transaction (`BEGIN`, `START TRANSACTION`).
    pub opens_transaction: bool,
    /// Some statement ends a transaction (`COMMIT`, `ROLLBACK`, `END`, `ABORT`;
    /// not `ROLLBACK TO SAVEPOINT` or `COMMIT PREPARED`).
    pub ends_transaction: bool,
    /// Some statement is a [`Statement::controls_transaction`] one.
    pub controls_transaction: bool,
    /// The text has a versioned executable comment (`/*!50000 … */`) that cannot be read
    /// one way: the server version is unknown ([`SqlLexOptions::server_version`]), or
    /// the form is one MySQL and MariaDB read differently (seven digits or more). The
    /// statements and flags above read such a comment as code; the text is never
    /// pageable and is [`danger`](Self::danger)ous. Fails closed: lexing is exact up to
    /// the first such comment, so it is always met, wherever the rest of the text goes.
    pub version_ambiguous: bool,
}

/// Why a driver refuses text under `payload.read_only` (see
/// [`Classification::read_only_refusal`]). `statement` is the 0-based index into
/// [`Classification::statements`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadOnlyRefusal {
    /// A statement that touches transaction state: it could end the READ ONLY
    /// scope the driver opened (`COMMIT`) or loosen it (`SET TRANSACTION READ WRITE`).
    TransactionControl { statement: usize },
    /// A script (several statements) in which a statement is not a plain read.
    ScriptWithNonRead { statement: usize },
    /// A single statement that is neither a plain read nor `EXPLAIN`/`DESCRIBE`/`ANALYZE`
    /// of one: a write, `CALL`, `KILL`, `FLUSH`, `SET GLOBAL`, `VACUUM INTO`, `SELECT ...
    /// INTO OUTFILE`, ...
    NotARead,
    /// A versioned comment (`/*!50000 … */`) that servers of different versions read as
    /// code or as a comment, and the two readings differ.
    AmbiguousVersionedComment,
}

impl std::fmt::Display for ReadOnlyRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TransactionControl { statement } => write!(
                f,
                "statement {} changes transaction state (COMMIT, ROLLBACK, BEGIN, SAVEPOINT, SET TRANSACTION, SET autocommit ...), which a read-only execution does not allow",
                statement + 1
            ),
            Self::ScriptWithNonRead { statement } => write!(
                f,
                "a read-only execution takes one statement, or a script of plain reads; statement {} is not a plain read",
                statement + 1
            ),
            Self::NotARead => write!(
                f,
                "a read-only execution takes a read (SELECT, WITH, VALUES, TABLE, SHOW, DESCRIBE) or EXPLAIN/ANALYZE of one; this statement is not one"
            ),
            Self::AmbiguousVersionedComment => {
                write!(f, "a read-only execution refuses text where {AMBIGUOUS_COMMENT}")
            }
        }
    }
}

impl std::error::Error for ReadOnlyRefusal {}

const AMBIGUOUS_COMMENT: &str = "a versioned comment (/*!NNNNN ... */) is read as code by some server versions and as a comment by others";

impl Classification {
    /// The rule for text run under `payload.read_only`: the driver wraps it in a READ
    /// ONLY scope, which a statement of the text must never be able to leave. So
    ///
    /// * any statement that controls the transaction is refused, and
    /// * a script of several statements is accepted only when every one is a plain
    ///   read ([`Statement::read_only`]);
    /// * a single statement is accepted when it is a plain read or wraps one
    ///   ([`Statement::wraps_statement`]: `EXPLAIN ANALYZE <read>`); a write fails
    ///   there too, so admin and session statements that need no transaction
    ///   (`KILL`, `FLUSH`, `SET GLOBAL`, `CALL`, `VACUUM INTO`, `ATTACH`, `SELECT ... INTO
    ///   OUTFILE`) are refused rather than left to the server's READ ONLY scope, which
    ///   does not stop them;
    /// * text whose versioned comments cannot be read one way is refused.
    ///
    /// Classify with the lexer options the SESSION uses, or this check and the
    /// server can split the text differently (`ANSI_QUOTES`, `NO_BACKSLASH_ESCAPES`,
    /// `standard_conforming_strings`).
    pub fn read_only_refusal(&self) -> Option<ReadOnlyRefusal> {
        if self.version_ambiguous {
            return Some(ReadOnlyRefusal::AmbiguousVersionedComment);
        }
        if let Some(statement) = self
            .statements
            .iter()
            .position(|statement| statement.controls_transaction)
        {
            return Some(ReadOnlyRefusal::TransactionControl { statement });
        }
        match self.statements.as_slice() {
            [] => None,
            [only] => {
                (!only.read_only && !only.wraps_statement).then_some(ReadOnlyRefusal::NotARead)
            }
            all => all
                .iter()
                .position(|statement| !statement.read_only)
                .map(|statement| ReadOnlyRefusal::ScriptWithNonRead { statement }),
        }
    }
}

impl Classification {
    /// No statement at all (empty, whitespace or comments only).
    pub fn is_empty(&self) -> bool {
        self.statements.is_empty()
    }

    /// The only statement, if there is exactly one.
    pub fn single(&self) -> Option<&Statement> {
        match self.statements.as_slice() {
            [statement] => Some(statement),
            _ => None,
        }
    }
}

/// Classifies `sql` for the dialect described by `options`.
pub fn classify(sql: &str, options: &SqlLexOptions) -> Classification {
    let mut lexer = Lexer::new(sql.as_bytes(), options);
    let mut result = Classification::default();
    let mut state = StatementState::new(0);
    loop {
        let token = lexer.next_token();
        match token.kind {
            TokenKind::End => {
                finish_statement(&mut state, sql.as_bytes(), sql.len(), &mut result);
                break;
            }
            TokenKind::Semicolon => {
                finish_statement(&mut state, sql.as_bytes(), token.start, &mut result);
                state = StatementState::new(token.end);
            }
            _ => state.push(sql.as_bytes(), &token),
        }
    }
    result.pageable = result.statements.len() == 1 && result.statements[0].read_only;
    if lexer.ambiguous {
        result.version_ambiguous = true;
        result.pageable = false;
        if result.danger.is_none() {
            result.danger = Some(Danger {
                statement: 0,
                reason: AMBIGUOUS_COMMENT,
            });
        }
    }
    result
}

fn finish_statement(
    state: &mut StatementState,
    bytes: &[u8],
    end: usize,
    result: &mut Classification,
) {
    if state.significant == 0 {
        return;
    }
    let statement = state.finish(bytes, end);
    let index = result.statements.len();
    if result.danger.is_none() {
        if let Some(reason) = statement.danger {
            result.danger = Some(Danger {
                statement: index,
                reason,
            });
        }
    }
    result.changes_database |= statement.changes_database;
    result.opens_transaction |= statement.opens_transaction;
    result.ends_transaction |= statement.ends_transaction;
    result.controls_transaction |= statement.controls_transaction;
    result.statements.push(statement);
}

/// What a [`SqlToken`] is. Comments and whitespace are not tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlTokenKind {
    /// A bare word: a keyword or an unquoted identifier. Compare its spelling
    /// case-insensitively.
    Word,
    /// A string literal or quoted identifier, quotes included. The first byte says which
    /// (`'`, `"`, `` ` ``, `[`, `$` or the `E` of an `E'…'` string).
    Quoted,
    /// A number: a digit and every letter, digit, `.` and `_` after it.
    Number,
    /// `(`
    Open,
    /// `)`
    Close,
    /// `;`
    Semicolon,
    /// `,`
    Comma,
    /// Any other single byte (an operator or punctuation character).
    Other,
}

/// One token of [`tokens`]: its kind and byte range in the input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlToken {
    pub kind: SqlTokenKind,
    pub range: Range<usize>,
}

/// Result of [`tokens`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Tokens {
    /// Every token of the input in order, `;` included, comments and whitespace skipped.
    pub tokens: Vec<SqlToken>,
    /// The text has an executable comment (`/*! … */`, `/*M! … */`) that the server runs:
    /// its content is in [`tokens`](Self::tokens), but the comment's own delimiters are
    /// not, so text edited around it is not safe to splice.
    pub executable_comment: bool,
    /// The input ends inside a string, quoted identifier, dollar quote or block comment.
    pub unterminated: bool,
}

/// Splits `sql` into tokens the way [`classify`] reads it for the dialect described by
/// `options`, for callers that edit a statement's text in place (find a clause by its
/// keyword at paren depth 0 and splice around it).
///
/// ```
/// use engine_driver_kit::sql::{tokens, SqlLexOptions, SqlTokenKind};
/// let scan = tokens("SELECT 'a;b' /* ; */ FROM t", &SqlLexOptions::postgres());
/// let kinds: Vec<_> = scan.tokens.iter().map(|token| token.kind).collect();
/// assert_eq!(kinds, [SqlTokenKind::Word, SqlTokenKind::Quoted, SqlTokenKind::Word, SqlTokenKind::Word]);
/// assert_eq!(&"SELECT 'a;b' /* ; */ FROM t"[scan.tokens[1].range.clone()], "'a;b'");
/// ```
pub fn tokens(sql: &str, options: &SqlLexOptions) -> Tokens {
    let mut lexer = Lexer::new(sql.as_bytes(), options);
    let mut tokens = Vec::new();
    while let Some(token) = next_public_token(&mut lexer) {
        tokens.push(token);
    }
    Tokens {
        tokens,
        executable_comment: lexer.executable_comment,
        unterminated: lexer.unterminated,
    }
}

fn next_public_token(lexer: &mut Lexer<'_>) -> Option<SqlToken> {
    let token = lexer.next_token();
    let kind = match token.kind {
        TokenKind::End => return None,
        TokenKind::Word => SqlTokenKind::Word,
        TokenKind::Quoted => SqlTokenKind::Quoted,
        TokenKind::Number => SqlTokenKind::Number,
        TokenKind::Open => SqlTokenKind::Open,
        TokenKind::Close => SqlTokenKind::Close,
        TokenKind::Semicolon => SqlTokenKind::Semicolon,
        TokenKind::Comma => SqlTokenKind::Comma,
        TokenKind::Other => SqlTokenKind::Other,
    };
    Some(SqlToken {
        kind,
        range: token.start..token.end,
    })
}

/// [`tokens`] one token at a time: the same tokens, read as far as the caller goes, with nothing
/// kept. For a caller that decides from the first words of a statement and must not pay for a
/// token list of a script of megabytes.
///
/// ```
/// use engine_driver_kit::sql::{token_iter, SqlLexOptions};
/// let options = SqlLexOptions::postgres();
/// assert_eq!(token_iter("SELECT 1; SELECT 2", &options).count(), 5);
/// ```
pub fn token_iter<'a>(sql: &'a str, options: &'a SqlLexOptions) -> TokenIter<'a> {
    TokenIter {
        lexer: Lexer::new(sql.as_bytes(), options),
    }
}

/// See [`token_iter`].
pub struct TokenIter<'a> {
    lexer: Lexer<'a>,
}

impl Iterator for TokenIter<'_> {
    type Item = SqlToken;

    fn next(&mut self) -> Option<SqlToken> {
        next_public_token(&mut self.lexer)
    }
}

/// What [`TokenStream::pull`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pull {
    /// The next token; its range is in the buffer that was given.
    Token(SqlToken),
    /// The buffer ends where the next token might go on (or begin): give it more input and
    /// pull again. Nothing was consumed.
    NeedMore,
    /// Nothing but whitespace and comments is left, and the input is complete.
    End,
}

/// [`tokens`] over input that arrives in pieces, for a script too large to hold: the caller
/// keeps the unconsumed tail of the input in a buffer, appends the next piece and pulls tokens
/// from where the last one ended. A token is returned only when the lexer read nothing at or
/// past the end of the buffer to decide it, so the tokens are the same whatever the pieces
/// are; otherwise [`Pull::NeedMore`] leaves everything as it was. Work is linear in the input
/// as long as the caller asks for at least as much again as the unfinished token holds.
///
/// ```
/// use engine_driver_kit::sql::{Pull, SqlLexOptions, SqlTokenKind, TokenStream};
/// let mut stream = TokenStream::new(SqlLexOptions::postgres());
/// let mut at = 0;
/// // The word may go on in the next piece.
/// assert_eq!(stream.pull(b"sel", &mut at, false), Pull::NeedMore);
/// let Pull::Token(token) = stream.pull(b"select 'a", &mut at, false) else { panic!() };
/// assert_eq!((token.kind, token.range, at), (SqlTokenKind::Word, 0..6, 6));
/// // The string is not closed yet.
/// assert_eq!(stream.pull(b"select 'a", &mut at, false), Pull::NeedMore);
/// let Pull::Token(token) = stream.pull(b"select 'a;b' ", &mut at, false) else { panic!() };
/// assert_eq!((token.kind, token.range), (SqlTokenKind::Quoted, 7..12));
/// ```
#[derive(Debug, Clone)]
pub struct TokenStream {
    options: SqlLexOptions,
    in_executable_comment: bool,
}

impl TokenStream {
    pub fn new(options: SqlLexOptions) -> Self {
        Self {
            options,
            in_executable_comment: false,
        }
    }

    /// The token of `buffer` that starts at or after `*at` (whitespace and comments skipped);
    /// `*at` moves to its end. `last`: no more input follows `buffer`, so its end ends
    /// whatever is open (an unterminated string is a token that runs to the end).
    pub fn pull(&mut self, buffer: &[u8], at: &mut usize, last: bool) -> Pull {
        let mut lexer = Lexer::new(buffer, &self.options);
        lexer.pos = *at;
        lexer.in_executable_comment = self.in_executable_comment;
        let token = lexer.next_token();
        if lexer.starved.get() && !last {
            return Pull::NeedMore;
        }
        self.in_executable_comment = lexer.in_executable_comment;
        *at = lexer.pos;
        let kind = match token.kind {
            TokenKind::End => return Pull::End,
            TokenKind::Word => SqlTokenKind::Word,
            TokenKind::Quoted => SqlTokenKind::Quoted,
            TokenKind::Number => SqlTokenKind::Number,
            TokenKind::Open => SqlTokenKind::Open,
            TokenKind::Close => SqlTokenKind::Close,
            TokenKind::Semicolon => SqlTokenKind::Semicolon,
            TokenKind::Comma => SqlTokenKind::Comma,
            TokenKind::Other => SqlTokenKind::Other,
        };
        Pull::Token(SqlToken {
            kind,
            range: token.start..token.end,
        })
    }
}

// ---------------------------------------------------------------- lexer

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenKind {
    Word,
    /// String literal or quoted identifier (content is never inspected).
    Quoted,
    Number,
    Open,
    Close,
    Semicolon,
    Comma,
    Other,
    End,
}

#[derive(Debug, Clone, Copy)]
struct Token {
    kind: TokenKind,
    start: usize,
    end: usize,
}

struct Lexer<'a> {
    bytes: &'a [u8],
    options: &'a SqlLexOptions,
    pos: usize,
    /// Inside `/*! … */`: a `*/` closes it and is skipped like whitespace.
    in_executable_comment: bool,
    /// A versioned comment that cannot be read one way was met (read as code from there).
    ambiguous: bool,
    /// An executable comment (`/*! … */`) that the server runs was entered.
    executable_comment: bool,
    /// The input ends inside a string, quoted identifier, dollar quote or block comment.
    unterminated: bool,
    /// The lexer looked for a byte at or past the end of the input: what it read last could
    /// read differently with more input behind it ([`TokenStream`] asks for more).
    starved: std::cell::Cell<bool>,
}

impl<'a> Lexer<'a> {
    fn new(bytes: &'a [u8], options: &'a SqlLexOptions) -> Self {
        Self {
            bytes,
            options,
            pos: 0,
            in_executable_comment: false,
            ambiguous: false,
            executable_comment: false,
            unterminated: false,
            starved: std::cell::Cell::new(false),
        }
    }

    fn peek(&self, offset: usize) -> Option<u8> {
        self.byte_at(self.pos + offset)
    }

    /// The byte at `index`; looking past the end marks the lexer starved.
    fn byte_at(&self, index: usize) -> Option<u8> {
        let byte = self.bytes.get(index).copied();
        if byte.is_none() {
            self.starved.set(true);
        }
        byte
    }

    fn token(&self, kind: TokenKind, start: usize) -> Token {
        Token {
            kind,
            start,
            end: self.pos,
        }
    }

    fn next_token(&mut self) -> Token {
        loop {
            let Some(byte) = self.peek(0) else {
                return self.token(TokenKind::End, self.bytes.len());
            };
            let start = self.pos;
            match byte {
                b if b.is_ascii_whitespace() => self.pos += 1,
                b'-' if self.peek(1) == Some(b'-') && self.dash_comment_starts() => {
                    self.skip_line();
                }
                b'#' if self.options.hash_comments => self.skip_line(),
                b'/' if self.peek(1) == Some(b'*') => {
                    if !self.start_block_comment() {
                        // `/*! … */` content is code: lexing continues inside it.
                        continue;
                    }
                }
                b'*' if self.in_executable_comment && self.peek(1) == Some(b'/') => {
                    self.in_executable_comment = false;
                    self.pos += 2;
                }
                b'\'' => {
                    self.pos += 1;
                    self.skip_quoted(b'\'', self.options.backslash_escapes);
                    return self.token(TokenKind::Quoted, start);
                }
                b'"' => {
                    self.pos += 1;
                    if self.options.double_quote_is_identifier {
                        self.skip_quoted(b'"', self.options.backslash_in_identifiers);
                    } else {
                        self.skip_quoted(b'"', self.options.backslash_escapes);
                    }
                    return self.token(TokenKind::Quoted, start);
                }
                b'[' if self.options.bracket_identifiers => {
                    self.pos += 1;
                    self.skip_quoted(b']', false);
                    return self.token(TokenKind::Quoted, start);
                }
                b'`' if self.options.backtick_identifiers => {
                    self.pos += 1;
                    self.skip_quoted(b'`', self.options.backslash_in_identifiers);
                    return self.token(TokenKind::Quoted, start);
                }
                b'$' if self.options.dollar_quotes && self.skip_dollar_quote() => {
                    return self.token(TokenKind::Quoted, start);
                }
                b'(' => {
                    self.pos += 1;
                    return self.token(TokenKind::Open, start);
                }
                b')' => {
                    self.pos += 1;
                    return self.token(TokenKind::Close, start);
                }
                b';' => {
                    self.pos += 1;
                    return self.token(TokenKind::Semicolon, start);
                }
                b',' => {
                    self.pos += 1;
                    return self.token(TokenKind::Comma, start);
                }
                b if is_word_start(b) => {
                    self.pos += 1;
                    while self.peek(0).is_some_and(is_word_continue) {
                        self.pos += 1;
                    }
                    // `E'…'`: a PostgreSQL escape string. The `E` is its prefix.
                    if self.options.e_strings
                        && self.pos - start == 1
                        && matches!(byte, b'e' | b'E')
                        && self.peek(0) == Some(b'\'')
                    {
                        self.pos += 1;
                        self.skip_quoted(b'\'', true);
                        return self.token(TokenKind::Quoted, start);
                    }
                    return self.token(TokenKind::Word, start);
                }
                b if b.is_ascii_digit() => {
                    self.pos += 1;
                    while self
                        .peek(0)
                        .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_')
                    {
                        self.pos += 1;
                    }
                    return self.token(TokenKind::Number, start);
                }
                _ => {
                    self.pos += 1;
                    return self.token(TokenKind::Other, start);
                }
            }
        }
    }

    fn dash_comment_starts(&self) -> bool {
        if !self.options.dash_comment_needs_space {
            return true;
        }
        // MySQL: `--` starts a comment only before whitespace/control or the end.
        self.peek(2)
            .is_none_or(|b| b.is_ascii_whitespace() || b.is_ascii_control())
    }

    fn skip_line(&mut self) {
        while let Some(byte) = self.peek(0) {
            self.pos += 1;
            if byte == b'\n' || (byte == b'\r' && self.options.line_comment_ends_at_cr) {
                break;
            }
        }
    }

    /// At `/*`. Skips an ordinary comment and returns `true`; for an executable
    /// comment that the server runs, skips only its opener (`/*!50000`) and returns
    /// `false`. A versioned one the server does not run (`/*!99999 … */` below that
    /// version) is an ordinary comment, closed by the first `*/`.
    fn start_block_comment(&mut self) -> bool {
        let mariadb_form = self.peek(2) == Some(b'M');
        let executable = self.options.executable_comments
            && !self.in_executable_comment
            && match self.peek(2) {
                Some(b'!') => true,
                Some(b'M') => self.options.mariadb_comments && self.peek(3) == Some(b'!'),
                _ => false,
            };
        if executable {
            let after_prefix = self.pos + if mariadb_form { 4 } else { 3 };
            let digits = self.bytes[after_prefix..]
                .iter()
                .take_while(|byte| byte.is_ascii_digit())
                .count();
            if after_prefix + digits == self.bytes.len() {
                // The digits may go on in the next piece of input.
                self.starved.set(true);
            }
            // Fewer than five digits are not a version: they are part of the code.
            let (runs, skip) = if digits < 5 {
                (true, 0)
            } else {
                (
                    self.versioned_runs(mariadb_form, after_prefix, digits),
                    digits,
                )
            };
            if runs {
                self.pos = after_prefix + skip;
                self.in_executable_comment = true;
                self.executable_comment = true;
                return false;
            }
        }
        self.pos += 2;
        let mut depth = 1_u32;
        while depth > 0 {
            match (self.peek(0), self.peek(1)) {
                (None, _) => {
                    self.unterminated = true;
                    return true;
                }
                (Some(b'*'), Some(b'/')) => {
                    self.pos += 2;
                    depth -= 1;
                }
                (Some(b'/'), Some(b'*')) if self.options.nested_block_comments => {
                    self.pos += 2;
                    depth += 1;
                }
                _ => self.pos += 1,
            }
        }
        true
    }

    /// Whether the server runs the versioned comment whose digits start at `at`. With a
    /// known server version: it runs when its version is not above the server's
    /// (measured on MySQL 8.4 and MariaDB 11.8; MariaDB also skips `/*!` versions
    /// 50700-99999, which are MySQL 5.7/8 only). Seven digits or more are read
    /// differently by MySQL and MariaDB, and an unknown server version cannot decide:
    /// the text is flagged ambiguous and read as code.
    fn versioned_runs(&mut self, mariadb_form: bool, at: usize, digits: usize) -> bool {
        if let (Some(server), true) = (self.options.server_version, digits <= 6) {
            let value = self.bytes[at..at + digits]
                .iter()
                .fold(0_u32, |value, digit| value * 10 + u32::from(digit - b'0'));
            let mysql_only = self.options.mariadb_comments
                && !mariadb_form
                && (50_700..100_000).contains(&value);
            return value <= server && !mysql_only;
        }
        self.ambiguous = true;
        true
    }

    /// After the opening quote: skips to just past the closing one. A doubled
    /// quote is an escaped quote; `backslash` makes `\x` skip `x`.
    fn skip_quoted(&mut self, quote: u8, backslash: bool) {
        while let Some(byte) = self.peek(0) {
            self.pos += 1;
            if backslash && byte == b'\\' {
                if self.peek(0).is_some() {
                    self.pos += 1;
                }
            } else if byte == quote {
                if self.peek(0) == Some(quote) {
                    self.pos += 1;
                } else {
                    return;
                }
            }
        }
        self.unterminated = true;
    }

    /// At `$`: skips a complete `$tag$ … $tag$` literal and returns `true`;
    /// returns `false` (consuming nothing) for `$1` parameters and lone `$`.
    fn skip_dollar_quote(&mut self) -> bool {
        let tag_start = self.pos + 1;
        let mut tag_end = tag_start;
        if let Some(first) = self.byte_at(tag_end) {
            if first.is_ascii_alphabetic() || first == b'_' || first >= 0x80 {
                tag_end += 1;
                while self
                    .byte_at(tag_end)
                    .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80)
                {
                    tag_end += 1;
                }
            }
        }
        if self.byte_at(tag_end) != Some(b'$') {
            return false;
        }
        // `$tag$`, including both dollars.
        let delimiter = &self.bytes[self.pos..=tag_end];
        let body_start = tag_end + 1;
        match find_subslice(&self.bytes[body_start..], delimiter) {
            Some(offset) => self.pos = body_start + offset + delimiter.len(),
            None => {
                self.pos = self.bytes.len();
                self.unterminated = true;
                self.starved.set(true);
            }
        }
        true
    }
}

fn is_word_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_' || byte >= 0x80
}

fn is_word_continue(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$' || byte >= 0x80
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

// ------------------------------------------------------------ statements

/// Upper-cases `word` into a stack buffer and maps it to a [`Keyword`].
fn keyword_of(word: &[u8]) -> Keyword {
    let mut buffer = [0_u8; 10];
    if word.len() > buffer.len() {
        return Keyword::Other;
    }
    let upper = &mut buffer[..word.len()];
    upper.copy_from_slice(word);
    upper.make_ascii_uppercase();
    match &*upper {
        b"SELECT" => Keyword::Select,
        b"WITH" => Keyword::With,
        b"VALUES" => Keyword::Values,
        b"TABLE" => Keyword::Table,
        b"SHOW" => Keyword::Show,
        b"EXPLAIN" => Keyword::Explain,
        b"DESCRIBE" | b"DESC" => Keyword::Describe,
        b"INSERT" => Keyword::Insert,
        b"UPDATE" => Keyword::Update,
        b"DELETE" => Keyword::Delete,
        b"REPLACE" => Keyword::Replace,
        b"MERGE" => Keyword::Merge,
        b"CREATE" => Keyword::Create,
        b"DROP" => Keyword::Drop,
        b"ALTER" => Keyword::Alter,
        b"TRUNCATE" => Keyword::Truncate,
        b"GRANT" => Keyword::Grant,
        b"REVOKE" => Keyword::Revoke,
        b"USE" => Keyword::Use,
        b"BEGIN" => Keyword::Begin,
        b"START" => Keyword::Start,
        b"COMMIT" => Keyword::Commit,
        b"ROLLBACK" => Keyword::Rollback,
        b"END" => Keyword::End,
        b"ABORT" => Keyword::Abort,
        b"ANALYZE" | b"ANALYSE" => Keyword::Analyze,
        b"SET" => Keyword::Set,
        b"SAVEPOINT" => Keyword::Savepoint,
        b"RELEASE" => Keyword::Release,
        b"PREPARE" => Keyword::Prepare,
        b"XA" => Keyword::Xa,
        b"LOCK" => Keyword::Lock,
        b"UNLOCK" => Keyword::Unlock,
        _ => Keyword::Other,
    }
}

/// A `SET` operand that names a transaction setting: `TRANSACTION`,
/// `transaction_read_only`, `default_transaction_isolation`, `autocommit`,
/// `tx_isolation`, `tx_read_only`.
fn names_transaction_setting(word: &[u8]) -> bool {
    let contains = |needle: &[u8]| {
        word.windows(needle.len())
            .any(|window| window.eq_ignore_ascii_case(needle))
    };
    contains(b"transaction")
        || contains(b"autocommit")
        || (word.len() > 3 && word[..3].eq_ignore_ascii_case(b"tx_"))
}

fn word_is(bytes: &[u8], range: &Range<usize>, expected: &[u8]) -> bool {
    bytes[range.clone()].eq_ignore_ascii_case(expected)
}

/// What the previous significant token was.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Prev {
    Nothing,
    Open,
    Close,
    Other,
}

/// A `DELETE`/`UPDATE` whose `WHERE` we are waiting for.
struct DmlFrame {
    depth: u32,
    keyword: Keyword,
    seen_where: bool,
}

struct StatementState {
    start: usize,
    significant: u32,
    prev: Prev,
    depth: u32,
    first: Option<(Keyword, Range<usize>)>,
    /// The keyword that decides what the statement does: the first one, or for
    /// `WITH` the first depth-0 DML/query keyword after the CTE list.
    main: Option<Keyword>,
    /// Words 2..4 of the statement (for `CREATE DATABASE`, `START TRANSACTION`).
    words: [Option<Range<usize>>; 3],
    word_count: u32,
    nested_write: bool,
    explain_analyze: bool,
    select_into: bool,
    /// An `INSERT`/`REPLACE` just after `(`: a write unless a `(` follows
    /// (the string functions `INSERT(…)` / `REPLACE(…)`).
    pending_write: bool,
    frames: Vec<DmlFrame>,
    danger: Option<&'static str>,
    /// Bit `d` is set while the body at paren depth `d` (>= 1) contains a nested
    /// `WITH`: a data-modifying statement may then follow plain words
    /// (`SEARCH DEPTH FIRST BY n SET ord DELETE …`, `CYCLE … USING path INSERT …`).
    nested_with_depths: u64,
    /// The previous word was `FOR`/`ON`/`KEY`/`DO`/`THEN`: the following
    /// `UPDATE`/`DELETE` is a lock or conflict clause, not a statement.
    prev_blocks_dml: bool,
    /// A `SET` statement names a transaction setting (or a quoted identifier).
    set_names_transaction: bool,
    /// An `OUTFILE` or `DUMPFILE` word: the statement writes a server-side file.
    file_output: bool,
    /// A `MERGE` was seen (first keyword, CTE main statement or nested).
    merge_seen: bool,
    /// Inside `WHEN … THEN` of a MERGE, an `AND <condition>` was seen.
    when_has_condition: bool,
    /// The previous word was a `THEN` without a condition.
    after_unconditional_then: bool,
}

impl StatementState {
    fn new(start: usize) -> Self {
        Self {
            start,
            significant: 0,
            prev: Prev::Nothing,
            depth: 0,
            first: None,
            main: None,
            words: [None, None, None],
            word_count: 0,
            nested_write: false,
            explain_analyze: false,
            select_into: false,
            pending_write: false,
            frames: Vec::new(),
            danger: None,
            nested_with_depths: 0,
            prev_blocks_dml: false,
            set_names_transaction: false,
            file_output: false,
            merge_seen: false,
            when_has_condition: false,
            after_unconditional_then: false,
        }
    }

    fn push(&mut self, bytes: &[u8], token: &Token) {
        self.significant += 1;
        if self.pending_write {
            self.pending_write = false;
            if token.kind != TokenKind::Open {
                self.nested_write = true;
            }
        }
        match token.kind {
            TokenKind::Open => {
                self.prev_blocks_dml = false;
                self.depth += 1;
                self.prev = Prev::Open;
            }
            TokenKind::Close => {
                self.prev_blocks_dml = false;
                if self.depth < u64::BITS {
                    self.nested_with_depths &= !(1 << self.depth);
                }
                self.depth = self.depth.saturating_sub(1);
                self.pop_frames_deeper_than(self.depth);
                self.prev = Prev::Close;
            }
            TokenKind::Word => {
                self.word(bytes, token);
                self.prev = Prev::Other;
            }
            _ => {
                self.prev_blocks_dml = false;
                // `SET "transaction_read_only" = off`, ``SET @@`autocommit` = 1``: a
                // quoted identifier can spell a transaction setting.
                if token.kind == TokenKind::Quoted
                    && matches!(self.first, Some((Keyword::Set, _)))
                    && matches!(bytes[token.start], b'"' | b'`')
                {
                    self.set_names_transaction = true;
                }
                self.prev = Prev::Other;
            }
        }
    }

    fn word(&mut self, bytes: &[u8], token: &Token) {
        let range = token.start..token.end;
        let keyword = keyword_of(&bytes[range.clone()]);
        if word_is(bytes, &range, b"OUTFILE") || word_is(bytes, &range, b"DUMPFILE") {
            self.file_output = true;
        }
        let blocked = std::mem::replace(
            &mut self.prev_blocks_dml,
            [&b"FOR"[..], b"ON", b"KEY", b"DO", b"THEN"]
                .iter()
                .any(|blocker| word_is(bytes, &range, blocker)),
        );
        let is_first = self.first.is_none();
        if is_first {
            self.first = Some((keyword, range.clone()));
            self.main = Some(keyword);
        } else if (self.word_count as usize) < self.words.len() {
            self.words[self.word_count as usize] = Some(range.clone());
            self.word_count += 1;
        }
        if !is_first
            && matches!(self.first, Some((Keyword::Set, _)))
            && names_transaction_setting(&bytes[range.clone()])
        {
            self.set_names_transaction = true;
        }
        if keyword == Keyword::Analyze {
            self.explain_analyze = true;
        }
        if keyword == Keyword::Merge {
            self.merge_seen = true;
        }
        if self.merge_seen {
            self.track_merge(bytes, &range, keyword);
        }
        let first = self.first.as_ref().map(|(keyword, _)| *keyword);
        // `WITH`: the main statement's keyword is the first depth-0 query/DML
        // keyword after the CTE list (CTE names are never reserved words).
        if first == Some(Keyword::With)
            && self.depth == 0
            && self.main == Some(Keyword::With)
            && matches!(
                keyword,
                Keyword::Select
                    | Keyword::Values
                    | Keyword::Table
                    | Keyword::Insert
                    | Keyword::Update
                    | Keyword::Delete
                    | Keyword::Merge
                    | Keyword::Replace
            )
        {
            self.main = Some(keyword);
            if matches!(keyword, Keyword::Update | Keyword::Delete) {
                self.push_frame(keyword);
            }
            return;
        }
        if is_first {
            if matches!(keyword, Keyword::Update | Keyword::Delete) {
                self.push_frame(keyword);
            }
            return;
        }
        // A data-modifying statement nested in parentheses (a CTE body, at any
        // depth and also after a nested `WITH` list). Only a word right after `(`
        // or `)` can start one: `FOR UPDATE`, `ON DUPLICATE KEY UPDATE`,
        // `DO UPDATE`, `THEN DELETE` and `SELECT insert(…)` cannot. After `)` it
        // is only a statement inside a CTE (`WITH y AS (…) DELETE …`).
        let in_cte = self.depth >= 1 && first == Some(Keyword::With);
        if in_cte && keyword == Keyword::With && self.depth < u64::BITS {
            self.nested_with_depths |= 1 << self.depth;
        }
        // After a nested WITH at this depth, plain words (SEARCH … SET ord,
        // CYCLE … USING path) may precede the statement: any DML keyword counts
        // unless it is a lock/conflict clause (`FOR UPDATE`, `DO UPDATE`, …).
        let after_nested_with = in_cte
            && self.depth < u64::BITS
            && self.nested_with_depths & (1 << self.depth) != 0
            && !blocked;
        if self.prev == Prev::Open || (in_cte && self.prev == Prev::Close) || after_nested_with {
            match keyword {
                Keyword::Update | Keyword::Delete => {
                    self.nested_write = true;
                    self.push_frame(keyword);
                }
                Keyword::Merge => self.nested_write = true,
                Keyword::Insert | Keyword::Replace => self.pending_write = true,
                _ => {}
            }
        }
        if word_is(bytes, &range, b"WHERE") {
            self.mark_where();
        } else if matches!(
            self.main,
            Some(Keyword::Select | Keyword::Table | Keyword::Values)
        ) && word_is(bytes, &range, b"INTO")
        {
            // `SELECT … INTO new_table`, `… INTO OUTFILE`: at any depth.
            self.select_into = true;
        }
    }

    /// Spots `MERGE … WHEN MATCHED THEN DELETE` with no extra condition.
    fn track_merge(&mut self, bytes: &[u8], range: &Range<usize>, keyword: Keyword) {
        if self.after_unconditional_then {
            self.after_unconditional_then = false;
            if keyword == Keyword::Delete && self.danger.is_none() {
                self.danger = Some(MERGE_DELETE);
            }
        }
        if word_is(bytes, range, b"WHEN") {
            self.when_has_condition = false;
        } else if word_is(bytes, range, b"AND") {
            self.when_has_condition = true;
        } else if word_is(bytes, range, b"THEN") {
            self.after_unconditional_then = !self.when_has_condition;
        }
    }

    fn push_frame(&mut self, keyword: Keyword) {
        self.frames.push(DmlFrame {
            depth: self.depth,
            keyword,
            seen_where: false,
        });
    }

    fn mark_where(&mut self) {
        let depth = self.depth;
        if let Some(frame) = self.frames.iter_mut().rev().find(|f| f.depth == depth) {
            frame.seen_where = true;
        }
    }

    fn pop_frames_deeper_than(&mut self, depth: u32) {
        while self.frames.last().is_some_and(|frame| frame.depth > depth) {
            if let Some(frame) = self.frames.pop() {
                self.frame_danger(&frame);
            }
        }
    }

    fn frame_danger(&mut self, frame: &DmlFrame) {
        if !frame.seen_where && self.danger.is_none() {
            self.danger = Some(match frame.keyword {
                Keyword::Delete => DELETE_WITHOUT_WHERE,
                _ => UPDATE_WITHOUT_WHERE,
            });
        }
    }

    fn word_at(&self, bytes: &[u8], index: usize, expected: &[u8]) -> bool {
        self.words
            .get(index)
            .and_then(Option::as_ref)
            .is_some_and(|range| word_is(bytes, range, expected))
    }

    fn finish(&mut self, bytes: &[u8], end: usize) -> Statement {
        while let Some(frame) = self.frames.pop() {
            self.frame_danger(&frame);
        }
        let (keyword, keyword_range) = match self.first.take() {
            Some((keyword, range)) => (Some(keyword), Some(range)),
            None => (None, None),
        };
        let read_only = !self.file_output
            && match (keyword, self.main) {
                (Some(Keyword::Explain), _) => !self.explain_analyze,
                (Some(Keyword::Show), _) => true,
                (Some(Keyword::Describe), _) => !self.explain_analyze,
                (Some(Keyword::Values | Keyword::Table), _) => !self.select_into,
                (Some(Keyword::Select), _) => !self.select_into && !self.nested_write,
                (Some(Keyword::With), Some(Keyword::Select | Keyword::Values | Keyword::Table)) => {
                    !self.select_into && !self.nested_write
                }
                _ => false,
            };
        let creates_database = keyword == Some(Keyword::Create)
            && (self.word_at(bytes, 0, b"DATABASE")
                || self.word_at(bytes, 0, b"SCHEMA")
                // MariaDB: CREATE OR REPLACE DATABASE
                || (self.word_at(bytes, 0, b"OR")
                    && self.word_at(bytes, 1, b"REPLACE")
                    && (self.word_at(bytes, 2, b"DATABASE") || self.word_at(bytes, 2, b"SCHEMA"))));
        let keyword_danger = match keyword {
            Some(Keyword::Drop) => Some("DROP permanently removes database objects"),
            Some(Keyword::Truncate) => Some("TRUNCATE removes every row of the table"),
            Some(Keyword::Alter) => Some("ALTER changes the definition of a database object"),
            Some(Keyword::Grant | Keyword::Revoke) => {
                Some("GRANT/REVOKE changes access privileges")
            }
            Some(Keyword::Create) if creates_database => {
                Some("CREATE DATABASE/SCHEMA creates a new database on the server")
            }
            _ => None,
        };
        let savepoint_or_prepared = self.word_at(bytes, 0, b"PREPARED")
            || self.word_at(bytes, 0, b"TO")
            || self.word_at(bytes, 1, b"TO");
        Statement {
            range: self.start..end,
            keyword,
            keyword_range,
            read_only,
            danger: keyword_danger.or(self.danger),
            changes_database: keyword == Some(Keyword::Use),
            opens_transaction: keyword == Some(Keyword::Begin)
                || (keyword == Some(Keyword::Start) && self.word_at(bytes, 0, b"TRANSACTION")),
            ends_transaction: match keyword {
                Some(Keyword::Commit | Keyword::Rollback) => !savepoint_or_prepared,
                Some(Keyword::End | Keyword::Abort) => true,
                _ => false,
            },
            // A READ ONLY transaction does not stop `INTO OUTFILE`/`DUMPFILE`, and MariaDB's
            // `ANALYZE ... INTO OUTFILE` really writes the file (MySQL's `EXPLAIN ANALYZE`
            // does not): a statement that names one is never a wrapper.
            wraps_statement: !self.file_output
                && match keyword {
                    Some(Keyword::Explain | Keyword::Describe) => true,
                    // `ANALYZE TABLE t` (MySQL) and PostgreSQL's `ANALYZE t` are not.
                    Some(Keyword::Analyze) => [
                        &b"FORMAT"[..],
                        b"SELECT",
                        b"WITH",
                        b"INSERT",
                        b"UPDATE",
                        b"DELETE",
                        b"REPLACE",
                    ]
                    .iter()
                    .any(|word| self.word_at(bytes, 0, word)),
                    _ => false,
                },
            controls_transaction: match keyword {
                Some(
                    Keyword::Begin
                    | Keyword::Start
                    | Keyword::Commit
                    | Keyword::Rollback
                    | Keyword::End
                    | Keyword::Abort
                    | Keyword::Savepoint
                    | Keyword::Release
                    | Keyword::Xa
                    | Keyword::Lock
                    | Keyword::Unlock,
                ) => true,
                Some(Keyword::Prepare) => self.word_at(bytes, 0, b"TRANSACTION"),
                Some(Keyword::Set) => self.set_names_transaction,
                _ => false,
            },
        }
    }
}

const DELETE_WITHOUT_WHERE: &str = "DELETE without WHERE removes every row of the table";
const MERGE_DELETE: &str = "MERGE … WHEN MATCHED THEN DELETE removes rows without a condition";
const UPDATE_WITHOUT_WHERE: &str = "UPDATE without WHERE changes every row of the table";

#[cfg(test)]
mod tests {
    use super::*;

    /// Every token of `text` pulled with the input arriving `piece` bytes at a time.
    fn streamed(text: &[u8], options: SqlLexOptions, piece: usize) -> Vec<(SqlTokenKind, Vec<u8>)> {
        let mut stream = TokenStream::new(options);
        let (mut buffer, mut fed, mut at) = (Vec::new(), 0, 0);
        let mut found = Vec::new();
        loop {
            let last = fed == text.len();
            match stream.pull(&buffer, &mut at, last) {
                Pull::Token(token) => found.push((token.kind, buffer[token.range].to_vec())),
                Pull::End => return found,
                Pull::NeedMore => {
                    assert!(!last, "the last piece never asks for more");
                    let next = (fed + piece).min(text.len());
                    buffer.extend_from_slice(&text[fed..next]);
                    fed = next;
                }
            }
        }
    }

    /// The same by the whole-text lexer.
    fn whole(text: &[u8], options: &SqlLexOptions) -> Vec<(SqlTokenKind, Vec<u8>)> {
        let text = std::str::from_utf8(text).unwrap();
        tokens(text, options).tokens.into_iter().map(|token| (token.kind, text.as_bytes()[token.range].to_vec())).collect()
    }

    #[test]
    fn pieces_of_any_size_give_the_tokens_of_the_whole_text() {
        let samples: [(&str, SqlLexOptions); 12] = [
            ("select 'it''s;ok', \"q;\"\"x\" from t; -- c;\n/* a /* b */ ; */ select 1.5e3 + $1;", SqlLexOptions::postgres()),
            ("create function f() returns int as $body$ begin return 1; end $body$ language plpgsql; select $$a$$, $a$b$a$;", SqlLexOptions::postgres()),
            ("select E'a\\'b;', e'\\\\' ; select 2 -- x\r; select 3", SqlLexOptions::postgres()),
            ("select 'a\\'b;c', `x;y`, \"s;\" # hash;\nselect 1 -- x\n;select 2--3;", SqlLexOptions::mysql()),
            ("/*!40101 SET NAMES utf8 */; /*!99999 hidden; */ select 1; /*!50000 select 2 */;", SqlLexOptions::mysql().with_server_version(80_036)),
            ("/*M!100000 select 1 */; /*!50700 select 2; */ select 3", SqlLexOptions::mariadb().with_server_version(110_800)),
            ("select [a;b], \"c\" from t; create trigger x begin select 1; end;", SqlLexOptions::sqlite()),
            ("select \u{e9}\u{4e16}\u{754c}, '\u{1f600};' ; \u{e9}t\u{e9}$x$y$x$", SqlLexOptions::postgres()),
            ("select 'unterminated; select 2", SqlLexOptions::postgres()),
            ("select $tag$ unterminated; select 2", SqlLexOptions::postgres()),
            ("select 1 /* unterminated; select 2", SqlLexOptions::mysql()),
            ("-", SqlLexOptions::mysql()),
        ];
        for (text, options) in samples {
            let expected = whole(text.as_bytes(), &options);
            for piece in [1, 2, 3, 5, 7, 16, 1000] {
                assert_eq!(streamed(text.as_bytes(), options, piece), expected, "{text:?} in pieces of {piece}");
            }
        }
    }

    fn pg(sql: &str) -> Classification {
        classify(sql, &SqlLexOptions::postgres())
    }
    fn my(sql: &str) -> Classification {
        classify(sql, &SqlLexOptions::mysql())
    }
    fn maria(sql: &str) -> Classification {
        classify(sql, &SqlLexOptions::mariadb())
    }
    fn lite(sql: &str) -> Classification {
        classify(sql, &SqlLexOptions::sqlite())
    }

    fn danger(c: &Classification) -> Option<&'static str> {
        c.danger.map(|d| d.reason)
    }

    // ---------------------------------------------------------- pageable

    #[test]
    fn plain_reads_are_pageable_in_every_dialect() {
        for sql in [
            "SELECT 1",
            "select * from t where a = 1",
            "  \n\t SELECT 1 ;  ",
            "SELECT 1;",
            "SELECT 1;;",
            "VALUES (1), (2)",
            "TABLE t",
            "SHOW TABLES",
            "SHOW search_path",
            "EXPLAIN SELECT * FROM t",
            "EXPLAIN INSERT INTO t VALUES (1)",
            "DESCRIBE t",
            "DESC t",
            "(SELECT 1) UNION (SELECT 2)",
            "((SELECT 1))",
            "SELECT * FROM t FOR UPDATE",
            "SELECT * FROM t FOR NO KEY UPDATE",
            "SELECT 'a;b', \"c;d\"",
        ] {
            for (name, c) in [("pg", pg(sql)), ("my", my(sql)), ("lite", lite(sql))] {
                assert!(c.pageable, "{name}: {sql:?} should be pageable: {c:?}");
                assert_eq!(c.danger, None, "{name}: {sql:?}");
            }
        }
    }

    #[test]
    fn writes_and_scripts_are_not_pageable() {
        for sql in [
            "INSERT INTO t VALUES (1)",
            "UPDATE t SET a = 1 WHERE id = 1",
            "DELETE FROM t WHERE id = 1",
            "REPLACE INTO t VALUES (1)",
            "MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN DELETE",
            "CREATE TABLE t (id int)",
            "CALL p()",
            "SET a = 1",
            "SELECT 1; SELECT 2",
            "SELECT 1; DELETE FROM t WHERE id = 1",
            "EXPLAIN ANALYZE SELECT 1",
            "EXPLAIN (ANALYZE, VERBOSE) SELECT 1",
            "EXPLAIN (analyse true) SELECT 1",
            "ANALYZE SELECT 1",
            "ANALYZE TABLE t",
            "SELECT * INTO new_t FROM t",
            "SELECT a INTO OUTFILE '/tmp/x' FROM t",
            "BEGIN",
            "",
            "   \n ",
            "-- only a comment",
            "/* only */ ;",
            ";;;",
        ] {
            assert!(!pg(sql).pageable, "pg {sql:?}");
            assert!(!my(sql).pageable, "my {sql:?}");
        }
    }

    #[test]
    fn string_functions_named_like_writes_are_reads() {
        for sql in [
            "SELECT REPLACE(c, 'a', 'b') FROM t",
            "SELECT INSERT('abc', 2, 1, 'X')",
            "SELECT (REPLACE(c, 'a', 'b')) FROM t",
            "SELECT * FROM (SELECT INSERT(a, 1, 1, 'x') AS a FROM t) s",
            "SELECT TRUNCATE(1.567, 2)",
            "SELECT UPDATE_COUNT, deleted, insert_at FROM t",
            "WITH x AS (SELECT REPLACE(c, 'a', 'b') AS c FROM t) SELECT * FROM x",
        ] {
            assert!(my(sql).pageable, "my {sql:?}");
            assert!(pg(sql).pageable, "pg {sql:?}");
            assert_eq!(my(sql).danger, None, "{sql:?}");
        }
    }

    #[test]
    fn with_is_pageable_only_when_ctes_do_not_write() {
        assert!(pg("WITH x AS (SELECT 1) SELECT * FROM x").pageable);
        assert!(pg("WITH RECURSIVE x(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM x WHERE n < 5) SELECT * FROM x").pageable);
        assert!(
            pg("WITH a AS (SELECT 1), b AS MATERIALIZED (SELECT 2) SELECT * FROM a, b").pageable
        );
        assert!(pg("WITH x AS (SELECT 1) VALUES (1)").pageable);
        for sql in [
            "WITH x AS (DELETE FROM t WHERE id = 1 RETURNING *) SELECT * FROM x",
            "WITH x AS (UPDATE t SET a = 1 WHERE id = 1 RETURNING *) SELECT * FROM x",
            "WITH x AS (INSERT INTO t VALUES (1) RETURNING *) SELECT * FROM x",
            "WITH x AS (MERGE INTO t USING s ON true WHEN MATCHED THEN DELETE RETURNING *) SELECT * FROM x",
            "WITH a AS (SELECT 1), x AS (INSERT INTO t VALUES (1) RETURNING *) SELECT * FROM x",
            "WITH x AS (SELECT 1) INSERT INTO t SELECT * FROM x",
            "WITH x AS (SELECT 1) DELETE FROM t WHERE id IN (SELECT * FROM x)",
            "WITH x AS (SELECT 1) UPDATE t SET a = 1 WHERE true",
            "WITH x AS (SELECT 1) SELECT * INTO new_t FROM x",
        ] {
            assert!(!pg(sql).pageable, "{sql:?}");
        }
    }

    // ------------------------------------------------ lexing: strings etc.

    #[test]
    fn keywords_inside_strings_identifiers_and_comments_are_inert() {
        for sql in [
            "SELECT 'DROP TABLE t; DELETE FROM t'",
            "SELECT \"drop\" FROM \"delete\"",
            "SELECT 1 -- DELETE FROM t\n",
            "SELECT 1 /* ; DROP TABLE t */",
            "SELECT 'it''s; DROP TABLE t'",
            "SELECT $$DROP TABLE t; DELETE FROM t$$",
            "SELECT $q$ it's ; DROP TABLE t $q$ , 1",
            "SELECT E'a\\'; DROP TABLE t'",
        ] {
            let c = pg(sql);
            assert!(c.pageable, "{sql:?}: {c:?}");
            assert_eq!(c.danger, None, "{sql:?}");
            assert_eq!(c.statements.len(), 1, "{sql:?}");
        }
        for sql in [
            "SELECT `drop`, `delete` FROM `use`",
            "SELECT 'a\\'; DROP TABLE t'",
            "SELECT \"x\\\"; DROP TABLE t\"",
            "SELECT 1 # DROP TABLE t\n",
            "SELECT 1 -- DROP TABLE t\n",
            "SELECT 1 /* DROP TABLE t */",
            "SELECT '\\\\'",
        ] {
            let c = my(sql);
            assert!(c.pageable, "{sql:?}: {c:?}");
            assert_eq!(c.danger, None, "{sql:?}");
            assert_eq!(c.statements.len(), 1, "{sql:?}");
        }
    }

    #[test]
    fn backslash_behaviour_follows_the_dialect() {
        let sql = r"SELECT 'a\'; DROP TABLE t; SELECT 'b'";
        // MySQL: `'` does not end the string, so the script is one statement.
        assert_eq!(my(sql).statements.len(), 1);
        // PostgreSQL (standard_conforming_strings): `\` is a plain character, the
        // string ends at the first quote and the rest is real SQL.
        let c = pg(sql);
        assert_eq!(c.statements.len(), 3);
        assert!(c.danger.is_some());
        // NO_BACKSLASH_ESCAPES makes MySQL read it like PostgreSQL.
        let c = classify(sql, &SqlLexOptions::mysql().without_backslash_escapes());
        assert_eq!(c.statements.len(), 3);
        assert!(c.danger.is_some());
        // PostgreSQL E'' strings do honour backslashes.
        assert_eq!(
            pg(r"SELECT E'a\'; DROP TABLE t; SELECT 'b'")
                .statements
                .len(),
            1
        );
        assert_eq!(
            pg(r"SELECT e'a\'; DROP TABLE t; SELECT 'b'")
                .statements
                .len(),
            1
        );
    }

    fn dialect_info(
        backslash: bool,
        double_quote: bool,
        bracket: bool,
    ) -> engine_driver_api::SqlDialectInfo {
        engine_driver_api::SqlDialectInfo {
            identifier_quote: "`".into(),
            string_literal_quote: "'".into(),
            supports_schemas: false,
            database_qualified_names: true,
            like_requires_text_cast: false,
            string_backslash_escapes: backslash,
            double_quote_identifiers: double_quote,
            bracket_identifiers: bracket,
        }
    }

    #[test]
    fn a_session_dialect_only_moves_away_from_the_preset() {
        let mysql = SqlLexOptions::mysql();
        // The default session leaves the preset as it is.
        assert_eq!(
            mysql.with_session_dialect(&dialect_info(true, false, false)),
            mysql
        );
        // `NO_BACKSLASH_ESCAPES`, `ANSI_QUOTES`, MariaDB `MSSQL`.
        assert_eq!(
            mysql.with_session_dialect(&dialect_info(false, false, false)),
            mysql.without_backslash_escapes()
        );
        assert_eq!(
            mysql.with_session_dialect(&dialect_info(true, true, false)),
            mysql.with_ansi_quotes()
        );
        let mssql = SqlLexOptions::mariadb().with_session_dialect(&dialect_info(true, true, true));
        assert!(mssql.double_quote_is_identifier && mssql.bracket_identifiers);
        assert!(mssql.backslash_escapes && mssql.mariadb_comments);
        // `false` never turns a preset's identifier quoting off (older drivers).
        let pg = SqlLexOptions::postgres();
        assert_eq!(
            pg.with_session_dialect(&dialect_info(false, false, false)),
            pg
        );
        let sqlite = SqlLexOptions::sqlite();
        assert_eq!(
            sqlite.with_session_dialect(&dialect_info(false, false, false)),
            sqlite
        );
        // Under ANSI_QUOTES the statement splits where the server splits it.
        let sql = r#"SELECT "a\"; DROP TABLE t; SELECT "b""#;
        let ansi = mysql.with_session_dialect(&dialect_info(true, true, false));
        assert_eq!(classify(sql, &mysql).statements.len(), 1);
        assert_eq!(classify(sql, &ansi).statements.len(), 3);
    }

    #[test]
    fn double_quotes_are_strings_in_mysql_and_identifiers_with_ansi_quotes() {
        let sql = r#"SELECT "a\"; DROP TABLE t; SELECT "b""#;
        // MySQL default: "…" is a string with backslash escapes.
        assert_eq!(my(sql).statements.len(), 1);
        // ANSI_QUOTES: "…" is an identifier, `\` is literal, doubling escapes.
        let ansi = SqlLexOptions::mysql().with_ansi_quotes();
        let c = classify(sql, &ansi);
        assert_eq!(c.statements.len(), 3);
        assert!(c.danger.is_some());
        assert_eq!(
            classify(r#"SELECT "a""; DROP TABLE t;""#, &ansi)
                .statements
                .len(),
            1
        );
        // PostgreSQL identifiers double the quote too.
        assert_eq!(
            pg(r#"SELECT "a""b; DROP TABLE t" FROM t"#).statements.len(),
            1
        );
    }

    #[test]
    fn postgres_block_comments_nest_and_mysql_ones_do_not() {
        let nested = "SELECT 1 /* a /* b */ ; DROP TABLE t; */ , 2";
        let c = pg(nested);
        assert_eq!(c.statements.len(), 1, "{c:?}");
        assert_eq!(c.danger, None);
        // MySQL: the first `*/` ends the comment, exposing `; DROP TABLE t; */ , 2`.
        let c = my(nested);
        assert_eq!(c.statements.len(), 3, "{c:?}");
        assert!(c.danger.is_some());
        // Unterminated comments swallow the rest.
        assert!(pg("SELECT 1 /* never closed ; DROP TABLE t").pageable);
        assert!(my("SELECT 1 /* never closed ; DROP TABLE t").pageable);
    }

    #[test]
    fn dollar_quotes_need_a_matching_tag() {
        assert!(pg("SELECT $a$ $b$ ; DROP TABLE t $a$").pageable);
        // A different tag does not close it.
        assert_eq!(
            pg("SELECT $a$ x $b$ ; DROP TABLE t $b$ ; SELECT 1")
                .statements
                .len(),
            1
        );
        // `$1` is a parameter, not a quote.
        let c = pg("SELECT $1, $2; DROP TABLE t");
        assert_eq!(c.statements.len(), 2);
        assert!(c.danger.is_some());
        // Not a dollar quote in MySQL (where `$` is just an identifier character).
        assert_eq!(my("SELECT $$x; DROP TABLE t$$").statements.len(), 2);
        // Unterminated.
        assert!(pg("SELECT $x$ DROP TABLE t").pageable);
        // `$` inside an identifier does not open a quote.
        assert_eq!(pg("SELECT a$b$c FROM t; DROP TABLE t").statements.len(), 2);
    }

    #[test]
    fn mysql_comment_styles() {
        // `#` comment.
        assert!(my("SELECT 1 # ; DROP TABLE t").pageable);
        // `-- ` needs a following space; `--5` is arithmetic and not a comment.
        assert!(my("SELECT 1 -- ; DROP TABLE t").pageable);
        assert!(my("SELECT 1 --\t; DROP TABLE t").pageable);
        assert_eq!(my("SELECT 1 --; DROP TABLE t").statements.len(), 2);
        assert_eq!(my("SELECT 5--5; DROP TABLE t").statements.len(), 2);
        // PostgreSQL: `--` always comments, `#` does not.
        assert!(pg("SELECT 1 --; DROP TABLE t").pageable);
        assert_eq!(pg("SELECT 1 # x; DROP TABLE t").statements.len(), 2);
    }

    // ------------------------------------------------------- USE detection

    #[test]
    fn use_is_detected_behind_comments_and_in_executable_comments() {
        for sql in [
            "USE prod",
            "use `prod`",
            "-- note\nUSE prod",
            "# note\nUSE prod",
            "/* note */ USE prod",
            "/*!50000 USE prod */",
            "/*! USE prod */",
            "SELECT 1; USE prod",
            "SELECT 1;\n-- x\nUSE prod;",
            "/*!40101 SET NAMES utf8 */; /*!50000 USE prod */",
        ] {
            let c = my(sql);
            assert!(c.changes_database, "{sql:?}: {c:?}");
            assert!(!c.pageable, "{sql:?}");
        }
        assert!(maria("/*M!100100 USE prod */").changes_database);
        // Plain comments hide it; PostgreSQL has no executable comments.
        assert!(!my("/* USE prod */ SELECT 1").changes_database);
        assert!(!my("-- USE prod\nSELECT 1").changes_database);
        assert!(!my("SELECT 'USE prod'").changes_database);
        assert!(!my("SELECT 1 AS `use`").changes_database);
        assert!(!pg("/*!50000 USE prod */ SELECT 1").changes_database);
        assert!(
            !my("/*M!100100 USE prod */").changes_database,
            "MariaDB-only comment"
        );
        assert!(!my("SELECT use_count FROM t").changes_database);
    }

    #[test]
    fn executable_comment_content_counts_as_code_and_closes_at_the_terminator() {
        let c = my("/*!50000 DROP TABLE t */ SELECT 1");
        assert!(c.danger.is_some());
        let c = classify(
            "SELECT /*!50000 SQL_NO_CACHE */ 1",
            &SqlLexOptions::mysql().with_server_version(80411),
        );
        assert!(c.pageable, "{c:?}");
        let c = my("SELECT 1 /*!50000 ; DELETE FROM t */");
        assert!(c.danger.is_some() && !c.pageable);
        // Optimizer hints are plain comments.
        assert!(my("SELECT /*+ MAX_EXECUTION_TIME(1000) */ 1").pageable);
    }

    // -------------------------------------------------------------- danger

    #[test]
    fn dangerous_statements_carry_a_reason() {
        for (sql, fragment) in [
            ("DROP TABLE t", "DROP"),
            ("drop database x", "DROP"),
            ("DROP SCHEMA s CASCADE", "DROP"),
            ("TRUNCATE t", "TRUNCATE"),
            ("TRUNCATE TABLE t", "TRUNCATE"),
            ("ALTER TABLE t ADD c int", "ALTER"),
            ("GRANT ALL ON t TO u", "GRANT"),
            ("REVOKE ALL ON t FROM u", "GRANT"),
            ("CREATE DATABASE d", "CREATE DATABASE"),
            ("CREATE SCHEMA s", "CREATE DATABASE"),
            ("CREATE OR REPLACE DATABASE d", "CREATE DATABASE"),
            ("DELETE FROM t", "DELETE without WHERE"),
            ("UPDATE t SET a = 1", "UPDATE without WHERE"),
            ("delete from t", "DELETE without WHERE"),
            ("DELETE FROM t ORDER BY id LIMIT 5", "DELETE without WHERE"),
            ("DELETE FROM t USING u", "DELETE without WHERE"),
        ] {
            for (name, c) in [("pg", pg(sql)), ("my", my(sql))] {
                let reason =
                    danger(&c).unwrap_or_else(|| panic!("{name}: {sql:?} must be dangerous"));
                assert!(reason.contains(fragment), "{name}: {sql:?} -> {reason}");
                assert_eq!(c.danger.unwrap().statement, 0);
                assert!(!c.pageable);
            }
        }
    }

    #[test]
    fn safe_statements_are_not_dangerous() {
        for sql in [
            "DELETE FROM t WHERE id = 1",
            "UPDATE t SET a = 1 WHERE id = 1",
            "DELETE FROM t USING u WHERE t.id = u.id",
            "UPDATE t SET a = (SELECT 1) WHERE id IN (SELECT id FROM u)",
            "INSERT INTO t VALUES (1)",
            "INSERT INTO t VALUES (1) ON CONFLICT (id) DO UPDATE SET a = 2",
            "INSERT INTO t VALUES (1) ON DUPLICATE KEY UPDATE a = 2",
            "CREATE TABLE t (id int, c int REFERENCES u ON DELETE CASCADE ON UPDATE CASCADE)",
            "CREATE INDEX i ON t (a)",
            "SELECT * FROM t FOR UPDATE",
            "MERGE INTO t USING s ON t.id = s.id WHEN MATCHED AND s.gone THEN DELETE WHEN MATCHED THEN UPDATE SET a = 1",
            "SELECT 1",
            "SET search_path = x",
            "SELECT 'DELETE FROM t'",
            "COMMENT ON TABLE t IS 'drop it'",
            "SELECT truncate(1.5, 0)",
        ] {
            assert_eq!(danger(&pg(sql)), None, "pg {sql:?}");
            assert_eq!(danger(&my(sql)), None, "my {sql:?}");
        }
    }

    #[test]
    fn where_only_counts_at_the_top_level_of_the_statement() {
        // The WHERE belongs to a subquery.
        for sql in [
            "UPDATE t SET a = (SELECT 1 WHERE true)",
            "DELETE FROM t USING (SELECT id FROM u WHERE x) s",
            "UPDATE t SET a = 1 FROM (SELECT id FROM u WHERE x = 1) s",
            "DELETE FROM t RETURNING (SELECT 1 WHERE true)",
        ] {
            assert!(pg(sql).danger.is_some(), "{sql:?}");
        }
        // The top-level WHERE comes after a subquery.
        for sql in [
            "DELETE FROM t USING (SELECT id FROM u) s WHERE t.id = s.id",
            "UPDATE t SET a = (SELECT 1) WHERE id = 1",
        ] {
            assert_eq!(danger(&pg(sql)), None, "{sql:?}");
        }
    }

    #[test]
    fn data_modifying_ctes_are_checked_for_where() {
        let c = pg("WITH x AS (DELETE FROM t RETURNING *) SELECT * FROM x");
        assert!(c.danger.is_some() && !c.pageable);
        let c = pg("WITH x AS (DELETE FROM t WHERE id = 1 RETURNING *) SELECT * FROM x");
        assert_eq!(c.danger, None);
        let c = pg("WITH x AS (SELECT 1) UPDATE t SET a = 1");
        assert!(danger(&c).unwrap().contains("UPDATE without WHERE"));
        let c = pg("WITH x AS (SELECT 1 WHERE true) DELETE FROM t WHERE id IN (SELECT * FROM x)");
        assert_eq!(c.danger, None);
        // WHERE inside the CTE does not protect the outer DELETE.
        let c = pg("WITH x AS (SELECT 1 WHERE true) DELETE FROM t");
        assert!(c.danger.is_some());
    }

    #[test]
    fn danger_is_found_in_any_statement_of_a_script() {
        let c = pg("SELECT 1; SELECT 2; DROP TABLE t");
        assert_eq!(c.danger.unwrap().statement, 2);
        assert_eq!(c.statements.len(), 3);
        let c = pg("DELETE FROM t WHERE id = 1; DELETE FROM t");
        assert_eq!(c.danger.unwrap().statement, 1);
        // The first danger wins.
        let c = pg("TRUNCATE a; DROP TABLE b");
        assert!(c.danger.unwrap().reason.contains("TRUNCATE"));
        assert_eq!(c.danger.unwrap().statement, 0);
        // A comment between statements changes nothing.
        let c = my("SELECT 1; -- c\n /* c */ DELETE FROM t");
        assert_eq!(c.danger.unwrap().statement, 1);
    }

    // ---------------------------------------------------------- statements

    #[test]
    fn statements_split_on_top_level_semicolons_only() {
        let sql = "SELECT 1; SELECT ';'; SELECT /* ; */ 2;\n-- ;\nSELECT 3";
        let c = pg(sql);
        assert_eq!(c.statements.len(), 4);
        assert_eq!(&sql[c.statements[1].range.clone()].trim(), &"SELECT ';'");
        assert!(c
            .statements
            .iter()
            .all(|s| s.keyword == Some(Keyword::Select)));
        // Empty statements are dropped.
        assert_eq!(pg(";; SELECT 1 ;; ;").statements.len(), 1);
        assert_eq!(pg("").statements.len(), 0);
        assert!(pg("   ").is_empty());
        assert!(pg("-- x").is_empty());
        assert!(my("# x\n/* y */").is_empty());
        assert_eq!(pg("(1)").statements[0].keyword, None);
    }

    #[test]
    fn first_keyword_skips_comments_whitespace_and_parentheses() {
        for sql in [
            "/* a */ -- b\n  select 1",
            "((select 1))",
            "(\n(SELECT 1)) UNION SELECT 2",
        ] {
            let c = pg(sql);
            let statement = c.single().unwrap();
            assert_eq!(statement.keyword, Some(Keyword::Select), "{sql:?}");
            let range = statement.keyword_range.clone().unwrap();
            assert!(sql[range].eq_ignore_ascii_case("select"));
        }
        assert_eq!(
            pg("WITH x AS (SELECT 1) SELECT 1").statements[0].keyword,
            Some(Keyword::With)
        );
        assert_eq!(
            pg("Explain Select 1").statements[0].keyword,
            Some(Keyword::Explain)
        );
        assert_eq!(
            pg("frobnicate x").statements[0].keyword,
            Some(Keyword::Other)
        );
    }

    // ------------------------------------------------------- transactions

    #[test]
    fn transaction_hints() {
        for sql in [
            "BEGIN",
            "begin",
            "BEGIN TRANSACTION",
            "BEGIN WORK",
            "BEGIN IMMEDIATE",
            "START TRANSACTION",
            "start transaction read only",
        ] {
            let c = pg(sql);
            assert!(c.opens_transaction && !c.ends_transaction, "{sql:?}");
        }
        for sql in [
            "COMMIT",
            "ROLLBACK",
            "END",
            "ABORT",
            "COMMIT WORK",
            "ROLLBACK WORK",
            "commit;",
        ] {
            let c = pg(sql);
            assert!(c.ends_transaction && !c.opens_transaction, "{sql:?}");
        }
        for sql in [
            "ROLLBACK TO SAVEPOINT s",
            "ROLLBACK TO s",
            "ROLLBACK WORK TO SAVEPOINT s",
            "COMMIT PREPARED 'x'",
            "ROLLBACK PREPARED 'x'",
            "SAVEPOINT s",
            "RELEASE SAVEPOINT s",
            "START SLAVE",
            "SELECT 'COMMIT'",
            "SELECT begin FROM t",
        ] {
            let c = my(sql);
            assert!(!c.ends_transaction && !c.opens_transaction, "{sql:?}");
        }
        let c = my("START TRANSACTION; UPDATE t SET a = 1 WHERE id = 1; COMMIT");
        assert!(c.opens_transaction && c.ends_transaction);
        assert!(c.statements[0].opens_transaction && c.statements[2].ends_transaction);
    }

    // ------------------------------------------- transaction control / read-only

    fn controls(c: &Classification) -> bool {
        c.controls_transaction && c.statements.iter().any(|s| s.controls_transaction)
    }

    #[test]
    fn transaction_control_statements_are_flagged_in_every_dialect() {
        for sql in [
            "BEGIN",
            "START TRANSACTION",
            "START TRANSACTION READ WRITE",
            "COMMIT",
            "commit work",
            "ROLLBACK",
            "ROLLBACK TO SAVEPOINT s",
            "ROLLBACK TO s",
            "COMMIT PREPARED 'x'",
            "ROLLBACK PREPARED 'x'",
            "PREPARE TRANSACTION 'x'",
            "END",
            "ABORT",
            "SAVEPOINT s",
            "RELEASE SAVEPOINT s",
            "RELEASE s",
            "XA START 'x'",
            "LOCK TABLES t READ",
            "UNLOCK TABLES",
            "SET TRANSACTION READ WRITE",
            "SET SESSION TRANSACTION READ WRITE",
            "SET LOCAL TRANSACTION ISOLATION LEVEL SERIALIZABLE",
            "SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE",
            "SET autocommit=1",
            "set AUTOCOMMIT = 0",
            "SET @@autocommit = 1",
            "SET @@session.autocommit = 1",
            "SET a = 1, b = 2, c = 3, d = 4, autocommit = 1",
            "SET transaction_read_only = off",
            "SET LOCAL transaction_read_only TO off",
            "SET default_transaction_read_only = off",
            "SET SESSION tx_read_only = 0",
            "SET @@tx_isolation = 'SERIALIZABLE'",
            r#"SET "transaction_read_only" = off"#,
            "SET @@`autocommit` = 1",
            "/*! COMMIT */",
            "/*!50000 COMMIT */",
            "/*!50000 SET autocommit = 1 */",
        ] {
            for (name, c) in [("pg", pg(sql)), ("my", my(sql)), ("maria", maria(sql))] {
                if name == "pg" && sql.starts_with("/*!") {
                    continue; // no executable comments in PostgreSQL
                }
                assert!(
                    controls(&c),
                    "{name}: {sql:?} must control the transaction: {c:?}"
                );
                assert!(
                    c.read_only_refusal().is_some(),
                    "{name}: {sql:?} must be refused read-only"
                );
            }
        }
        // MariaDB-only executable comment.
        assert!(controls(&maria("/*M!100100 COMMIT */")));
        assert!(!controls(&my("/*M!100100 COMMIT */")));
    }

    #[test]
    fn plain_reads_and_other_statements_do_not_control_the_transaction() {
        for sql in [
            "SELECT 1",
            "SELECT 'COMMIT'",
            "SELECT begin, `end`, \"release\" FROM t",
            "SELECT * FROM t FOR UPDATE",
            "SELECT * FROM t LOCK IN SHARE MODE",
            "SELECT set_config('a', 'b', false)",
            "WITH x AS (SELECT 1) SELECT * FROM x",
            "EXPLAIN ANALYZE SELECT 1",
            "EXPLAIN ANALYZE DELETE FROM t WHERE id = 1",
            "EXPLAIN (ANALYZE, BUFFERS) UPDATE t SET a = 1 WHERE id = 1",
            "SHOW TABLES",
            "DESCRIBE t",
            "-- COMMIT\nSELECT 1",
            "/* COMMIT */ SELECT 1",
            "SELECT 1 # COMMIT",
        ] {
            let c = my(sql);
            assert!(!controls(&c), "my {sql:?}: {c:?}");
            assert_eq!(c.read_only_refusal(), None, "my {sql:?}");
        }
        for sql in [
            "SELECT 1; SELECT 2",
            "SELECT 1; WITH x AS (SELECT 2) SELECT * FROM x; SHOW TABLES",
            "SELECT 1;",
            "EXPLAIN ANALYZE DELETE FROM t WHERE id = 1",
            "EXPLAIN (ANALYZE, FORMAT JSON) SELECT 1",
        ] {
            assert_eq!(pg(sql).read_only_refusal(), None, "pg {sql:?}");
        }
        // MariaDB `ANALYZE <statement>` wraps one; `ANALYZE TABLE` does not.
        for sql in [
            "ANALYZE FORMAT=JSON SELECT * FROM t",
            "ANALYZE SELECT 1",
            "ANALYZE DELETE FROM t WHERE id = 1",
        ] {
            assert_eq!(maria(sql).read_only_refusal(), None, "{sql}");
        }
        for sql in ["ANALYZE TABLE t", "ANALYZE t", "ANALYZE VERBOSE t"] {
            assert_eq!(
                maria(sql).read_only_refusal(),
                Some(ReadOnlyRefusal::NotARead),
                "{sql}"
            );
        }
        assert!(!controls(&pg("/*!50000 COMMIT */ SELECT 1")));
    }

    /// A single statement that is not a read is refused up front, so the admin and session
    /// statements the server's READ ONLY scope does not stop never run.
    #[test]
    fn read_only_refuses_a_single_statement_that_is_not_a_read() {
        for sql in [
            "INSERT INTO t VALUES (1)",
            "DELETE FROM t WHERE id = 1",
            "UPDATE t SET a = 1 WHERE id = 1",
            "CREATE TABLE u (a int)",
            "DROP TABLE t",
            "CALL p()",
            "DO $$ BEGIN COMMIT; END $$",
            "EXECUTE s",
            "KILL 12",
            "KILL QUERY 12",
            "FLUSH TABLES",
            "SET GLOBAL read_only = 0",
            "SET @@global.read_only = 0",
            "SET search_path = public",
            "SET sql_mode = ''",
            "SET @a = 1",
            "VACUUM INTO '/tmp/x.db'",
            "VACUUM",
            "ATTACH 'x.db' AS x",
            "PRAGMA query_only = OFF",
            "SELECT * INTO OUTFILE '/tmp/x' FROM t",
            "SELECT * INTO new_t FROM t",
            "USE other",
            "LOAD DATA INFILE '/tmp/x' INTO TABLE t",
            "SHUTDOWN",
            "COPY t TO PROGRAM 'id'",
            "WITH x AS (DELETE FROM t RETURNING *) SELECT * FROM x",
        ] {
            for (name, c) in [("pg", pg(sql)), ("my", my(sql)), ("lite", lite(sql))] {
                if controls(&c) {
                    continue; // refused as transaction control
                }
                assert_eq!(
                    c.read_only_refusal(),
                    Some(ReadOnlyRefusal::NotARead),
                    "{name}: {sql:?}"
                );
            }
        }
    }

    #[test]
    fn read_only_refuses_a_script_with_a_statement_that_is_not_a_plain_read() {
        for (sql, statement) in [
            ("SELECT 1; DELETE FROM t WHERE id = 1", 1),
            ("DELETE FROM t WHERE id = 1; SELECT 1", 0),
            ("SELECT 1; SELECT 2; INSERT INTO t VALUES (1)", 2),
            ("SELECT 1; SET search_path = x", 1),
            ("SELECT 1; EXPLAIN ANALYZE SELECT 2", 1),
            ("SELECT 1; SELECT * INTO u FROM t", 1),
        ] {
            assert_eq!(
                pg(sql).read_only_refusal(),
                Some(ReadOnlyRefusal::ScriptWithNonRead { statement }),
                "{sql:?}"
            );
        }
        for (sql, statement) in [
            ("SELECT 1; COMMIT", 1),
            ("COMMIT; DELETE FROM t WHERE id = 1", 0),
            ("SELECT 1; SELECT 2; END", 2),
            ("SELECT 1; ABORT", 1),
            ("SELECT 1; SET autocommit = 1", 1),
            ("SELECT 1; RELEASE SAVEPOINT s", 1),
            ("SELECT 1; ROLLBACK TO SAVEPOINT s", 1),
            ("SELECT 1; SET TRANSACTION READ WRITE", 1),
        ] {
            assert_eq!(
                pg(sql).read_only_refusal(),
                Some(ReadOnlyRefusal::TransactionControl { statement }),
                "{sql:?}"
            );
        }
        let message = ReadOnlyRefusal::TransactionControl { statement: 1 }.to_string();
        assert!(
            message.contains("statement 2") && message.contains("transaction"),
            "{message}"
        );
    }

    /// The reported escape: `"a\" FROM (SELECT 1 AS "a\") x; COMMIT; DELETE ...; -- "`
    /// is ONE statement to a lexer that reads `"…"` as a string with backslash
    /// escapes, and three to a server in `ANSI_QUOTES`. Only the session's own options
    /// see the `COMMIT`.
    #[test]
    fn read_only_needs_the_session_lexer_options() {
        let probe =
            r#"SELECT "a\" FROM (SELECT 1 AS "a\") x; COMMIT; DELETE FROM t WHERE id=1; -- ""#;
        let default = classify(probe, &SqlLexOptions::mysql());
        assert_eq!(
            default.statements.len(),
            1,
            "the default options see one statement"
        );
        assert_eq!(default.read_only_refusal(), None);
        for options in [
            SqlLexOptions::mysql().with_ansi_quotes(),
            SqlLexOptions::mariadb().with_ansi_quotes(),
            SqlLexOptions::mysql()
                .with_ansi_quotes()
                .without_backslash_escapes(),
        ] {
            let c = classify(probe, &options);
            assert_eq!(c.statements.len(), 3, "{options:?}");
            assert_eq!(
                c.read_only_refusal(),
                Some(ReadOnlyRefusal::TransactionControl { statement: 1 }),
                "{options:?}"
            );
        }
        // NO_BACKSLASH_ESCAPES: `'\'` is a whole string, the rest is real SQL.
        let probe = r"SELECT '\'; COMMIT; DELETE FROM t WHERE id = 1";
        assert_eq!(classify(probe, &SqlLexOptions::mysql()).statements.len(), 1);
        let c = classify(probe, &SqlLexOptions::mysql().without_backslash_escapes());
        assert_eq!(
            c.read_only_refusal(),
            Some(ReadOnlyRefusal::TransactionControl { statement: 1 })
        );
        // PostgreSQL: `'\'` is a whole string (standard_conforming_strings = on) but
        // E'' and dollar quotes keep their own rules.
        let c = pg(probe);
        assert_eq!(
            c.read_only_refusal(),
            Some(ReadOnlyRefusal::TransactionControl { statement: 1 })
        );
        for sql in [
            r"SELECT E'\'; COMMIT; DELETE FROM t WHERE id = 1; SELECT '",
            "SELECT $$; COMMIT; $$",
            "SELECT $q$ ' ; COMMIT; DELETE FROM t $q$ , 1",
            "SELECT 1 /* a /* b */ ; COMMIT; */",
        ] {
            assert_eq!(pg(sql).statements.len(), 1, "{sql:?}");
            assert_eq!(pg(sql).read_only_refusal(), None, "{sql:?}");
        }
        // The same text with the quotes undone is a real script.
        assert_eq!(
            pg("SELECT $$x$$; COMMIT; DELETE FROM t WHERE id = 1").read_only_refusal(),
            Some(ReadOnlyRefusal::TransactionControl { statement: 1 })
        );
        assert_eq!(
            pg("SELECT 1 /* a */ ; COMMIT").read_only_refusal(),
            Some(ReadOnlyRefusal::TransactionControl { statement: 1 })
        );
    }

    // ------------------------------------------------ versioned comments

    fn my_v(sql: &str, version: u32) -> Classification {
        classify(sql, &SqlLexOptions::mysql().with_server_version(version))
    }
    fn maria_v(sql: &str, version: u32) -> Classification {
        classify(sql, &SqlLexOptions::mariadb().with_server_version(version))
    }

    /// `SELECT 1 /*!99999 ' */ ; COMMIT; DELETE ...; -- '`: a server older than 9.99.99
    /// reads the comment as a comment, so `COMMIT` and `DELETE` are real statements; one
    /// that is not reads a string. The lexer must not guess.
    #[test]
    fn a_versioned_comment_above_the_server_version_is_a_plain_comment() {
        let probe = "SELECT 1 /*!99999 ' */ ; COMMIT; DELETE FROM t; -- '";
        for (name, c) in [
            ("my 8.4.11", my_v(probe, 80411)),
            ("maria 11.8.9", maria_v(probe, 110809)),
        ] {
            assert_eq!(c.statements.len(), 3, "{name}: {c:?}");
            assert!(c.danger.is_some(), "{name}: the hidden DELETE is dangerous");
            assert_eq!(
                c.read_only_refusal(),
                Some(ReadOnlyRefusal::TransactionControl { statement: 1 }),
                "{name}"
            );
            assert!(!c.version_ambiguous, "{name}");
        }
        // A server new enough to run it reads a string: one statement, as the server does.
        let c = my_v(probe, 99999);
        assert_eq!(c.statements.len(), 1, "{c:?}");
        assert_eq!(c.read_only_refusal(), None);
        // MariaDB's own form.
        let probe = "SELECT 1 /*M!999999 ' */ ; COMMIT; DELETE FROM t; -- '";
        assert_eq!(maria_v(probe, 110809).statements.len(), 3);
        assert_eq!(maria_v(probe, 999999).statements.len(), 1);
        // MySQL does not know `/*M!`: always a comment.
        assert_eq!(my_v(probe, 999_999).statements.len(), 3);
    }

    #[test]
    fn a_versioned_comment_runs_when_its_version_is_not_above_the_server() {
        for (sql, version, runs) in [
            ("/*!50000 COMMIT */", 80411, true),
            ("/*!80411 COMMIT */", 80411, true),
            ("/*!080411 COMMIT */", 80411, true),
            ("/*!80412 COMMIT */", 80411, false),
            ("/*!080412 COMMIT */", 80411, false),
            ("/*!99999 COMMIT */", 80411, false),
            ("/*!100000 COMMIT */", 80411, false),
            ("/*!50000 COMMIT */", 40000, false),
            ("/*!40000 COMMIT */", 40000, true),
            ("/*!COMMIT */", 40000, true),
            // Fewer than five digits are not a version: they are code.
            ("/*!1 COMMIT */", 40000, true),
            ("/*!1234 COMMIT */", 40000, true),
        ] {
            let c = my_v(sql, version);
            assert_eq!(controls(&c), runs, "mysql {version}: {sql}");
            assert_eq!(
                c.read_only_refusal().is_some(),
                runs,
                "mysql {version}: {sql}"
            );
        }
        for (sql, version, runs) in [
            ("/*M!110809 COMMIT */", 110809, true),
            ("/*M!110810 COMMIT */", 110809, false),
            ("/*M!100000 COMMIT */", 110809, true),
            ("/*M!999999 COMMIT */", 110809, false),
            ("/*M!80000 COMMIT */", 110809, true),
            ("/*M! COMMIT */", 110809, true),
            ("/*!50000 COMMIT */", 110809, true),
            ("/*!50699 COMMIT */", 110809, true),
            // MariaDB skips the MySQL 5.7 / 8 versions of `/*!`.
            ("/*!50700 COMMIT */", 110809, false),
            ("/*!80000 COMMIT */", 110809, false),
            ("/*!99999 COMMIT */", 110809, false),
            ("/*!100000 COMMIT */", 110809, true),
            ("/*!110809 COMMIT */", 110809, true),
            ("/*!110810 COMMIT */", 110809, false),
        ] {
            let c = maria_v(sql, version);
            assert_eq!(controls(&c), runs, "mariadb {version}: {sql}");
        }
        // The content of a comment that does not run is not looked at.
        let c = my_v("SELECT 1 /*!99999 ; DROP TABLE t */", 80411);
        assert!(c.pageable && c.danger.is_none(), "{c:?}");
        let c = my_v("SELECT 1 /*!50000 ; DROP TABLE t */", 80411);
        assert!(c.danger.is_some() && !c.pageable, "{c:?}");
        // After the comment ends, lexing goes on as code.
        let c = my_v("/*!99999 x */ COMMIT", 80411);
        assert!(controls(&c));
    }

    /// Fails closed: with no server version, any versioned comment makes the text ambiguous,
    /// wherever it sits (also behind other comments and strings): lexing is exact up to
    /// the first such comment, so it is always met.
    #[test]
    fn with_an_unknown_version_any_versioned_comment_makes_the_text_ambiguous() {
        for options in [SqlLexOptions::mysql(), SqlLexOptions::mariadb()] {
            for sql in [
                "SELECT 1 /*!99999 ' */ ; COMMIT; DELETE FROM t; -- '",
                // The reported dodge: the second comment sits inside a string when the
                // first one is read as code.
                "SELECT 1 /*!90000 ' */ /*!50000 ; COMMIT; DELETE FROM t WHERE 1 */",
                "/*!50000 COMMIT */",
                "/*!50000 USE prod */",
                "/*!40101 SET NAMES utf8 */",
                "SELECT /*!50000 SQL_NO_CACHE */ 1",
                "SELECT 1 /*!99999 */",
                "SELECT 1 /*!50000 ' */ ; COMMIT; /*!50001 ' */ ; DELETE FROM t",
            ] {
                let c = classify(sql, &options);
                assert!(c.version_ambiguous, "{sql}: {c:?}");
                assert!(!c.pageable, "{sql}");
                assert!(
                    c.danger.is_some(),
                    "{sql}: an ambiguous text needs confirmation"
                );
                assert_eq!(
                    c.read_only_refusal(),
                    Some(ReadOnlyRefusal::AmbiguousVersionedComment),
                    "{sql}"
                );
            }
        }
        // MariaDB's own form (MySQL reads `/*M!` as a plain comment).
        let c = maria("SELECT 1 /*M!999999 ' */ ; COMMIT; DELETE FROM t; -- '");
        assert!(c.version_ambiguous);
        assert!(!my("SELECT 1 /*M!999999 ' */ ; COMMIT; DELETE FROM t; -- '").version_ambiguous);
        // A hidden comment after many others, each of which alone is harmless.
        let many = format!(
            "{}SELECT 1 /*!90000 ' */ /*!50000 ; COMMIT; DELETE FROM t WHERE 1 */",
            "SELECT 1 /* x */; ".repeat(40)
        );
        assert!(my(&many).version_ambiguous);
        let many_hints = format!("SELECT 1 {}", "/*!50000 */ ".repeat(40));
        assert!(my(&many_hints).version_ambiguous);
        // No versioned comment (or one with fewer than five digits, which is code): decided.
        for sql in [
            "SELECT 1 /* ordinary */",
            "SELECT 1 /*+ MAX_EXECUTION_TIME(1000) */",
            "SELECT /*!1234 SQL_NO_CACHE */ 1",
            "SELECT /*! SQL_NO_CACHE */ 1",
            "SELECT 1 /*M!100000 ; COMMIT */",
        ] {
            let c = my(sql);
            assert!(!c.version_ambiguous, "{sql}: {c:?}");
        }
        // PostgreSQL has no executable comments.
        assert!(!pg("SELECT 1 /*!50000 ; COMMIT */").version_ambiguous);
    }

    /// Seven digits or more are read differently by MySQL and MariaDB: ambiguous even with
    /// the version known; so are two of them together (the reported dodge).
    #[test]
    fn seven_digit_versions_are_ambiguous_even_with_a_known_version() {
        for sql in [
            "SELECT 1 /*!1234567 ' */ ; COMMIT; DELETE FROM t; -- '",
            "SELECT 1 /*!9999999 ' */ /*!0000000 ; COMMIT; DELETE FROM t WHERE 1 */",
        ] {
            for (name, c) in [
                ("my 8.4.11", my_v(sql, 80411)),
                ("maria 11.8.9", maria_v(sql, 110809)),
            ] {
                assert!(c.version_ambiguous, "{name}: {sql}");
                assert_eq!(
                    c.read_only_refusal(),
                    Some(ReadOnlyRefusal::AmbiguousVersionedComment),
                    "{name}: {sql}"
                );
            }
        }
    }

    #[test]
    fn mariadb_seven_digit_m_comments_are_ambiguous() {
        let sql = "SELECT 1 /*M!9999999 ' */ /*M!0000000 ; COMMIT; DELETE FROM t WHERE 1 */";
        let c = maria_v(sql, 110809);
        assert!(c.version_ambiguous, "{c:?}");
        assert_eq!(
            c.read_only_refusal(),
            Some(ReadOnlyRefusal::AmbiguousVersionedComment)
        );
    }

    /// With the version known, comments are decided and harmless hints stay pageable.
    #[test]
    fn with_a_known_version_decided_comments_are_not_ambiguous() {
        for sql in [
            "SELECT /*!50000 SQL_NO_CACHE */ 1",
            "SELECT 1 /*!99999 */",
            "SELECT 1 /*!99999 ; DROP TABLE t */",
        ] {
            let c = my_v(sql, 80411);
            assert!(
                !c.version_ambiguous && c.pageable && c.danger.is_none(),
                "{sql}: {c:?}"
            );
        }
        assert!(my_v("/*!50000 COMMIT */", 80411)
            .read_only_refusal()
            .is_some());
        // The reported probe, decided: the first comment is a comment, the second code.
        let c = my_v(
            "SELECT 1 /*!90000 ' */ /*!50000 ; COMMIT; DELETE FROM t WHERE 1 */",
            80411,
        );
        assert!(!c.version_ambiguous);
        assert_eq!(c.statements.len(), 3, "{c:?}");
        assert_eq!(
            c.read_only_refusal(),
            Some(ReadOnlyRefusal::TransactionControl { statement: 1 })
        );
    }

    /// A READ ONLY transaction does not stop `INTO OUTFILE`/`DUMPFILE`: a wrapper that names
    /// one is refused (MariaDB's `ANALYZE ... INTO OUTFILE` writes the file; MySQL's
    /// `EXPLAIN ANALYZE ... INTO OUTFILE` does not, measured on 8.4.11 and 11.8.9).
    #[test]
    fn a_wrapper_that_writes_a_file_is_not_a_wrapper() {
        for sql in [
            "EXPLAIN ANALYZE SELECT 1 INTO OUTFILE '/tmp/x'",
            "EXPLAIN SELECT 1 INTO OUTFILE '/tmp/x'",
            "EXPLAIN ANALYZE SELECT 1 INTO DUMPFILE '/tmp/x'",
            "ANALYZE SELECT 1 INTO OUTFILE '/tmp/x'",
            "ANALYZE FORMAT=JSON SELECT 1 INTO DUMPFILE '/tmp/x'",
            "DESCRIBE SELECT 1 INTO OUTFILE '/tmp/x'",
            "EXPLAIN ANALYZE SELECT a FROM t into outfile '/tmp/x' FIELDS TERMINATED BY ','",
        ] {
            for (name, c) in [("my", my(sql)), ("maria", maria(sql))] {
                assert_eq!(
                    c.read_only_refusal(),
                    Some(ReadOnlyRefusal::NotARead),
                    "{name}: {sql}"
                );
            }
        }
        // The word as a string or identifier is just text.
        for sql in [
            "EXPLAIN ANALYZE SELECT 'outfile' FROM t",
            "EXPLAIN ANALYZE SELECT `outfile` FROM t",
        ] {
            assert_eq!(my(sql).read_only_refusal(), None, "{sql}");
        }
    }

    // ------------------------------------------------------------- misc

    #[test]
    fn sqlite_dialect_reads_backticks_and_double_quotes() {
        assert!(lite("SELECT `a;b`, \"c;d\" FROM t").pageable);
        assert!(lite("SELECT 1 -- ; DROP TABLE t").pageable);
        assert!(
            !lite("SELECT 1 /* a /* b */ ; DROP TABLE t */").pageable,
            "no nesting"
        );
    }

    #[test]
    fn non_ascii_and_odd_bytes_do_not_panic() {
        for sql in [
            "SELECT 'é;' FROM tâble; DROP TABLE t",
            "SELECT \u{1F600} FROM t",
            "SELECT '",
            "SELECT \"",
            "SELECT `",
            "SELECT $",
            "SELECT $$",
            "SELECT /*",
            "SELECT /*!",
            "SELECT /*!50000",
            "SELECT --",
            "SELECT E'",
            "SELECT \\",
            "((((",
            "))))",
            "\0",
        ] {
            let _ = pg(sql);
            let _ = my(sql);
            let _ = maria(sql);
        }
        // Deeply unbalanced parentheses are fine.
        assert!(!pg(")))) DELETE FROM t").pageable);
    }

    #[test]
    fn long_inputs_classify_in_one_pass() {
        // 200k tokens: guards against accidental quadratic behaviour.
        let sql = format!("SELECT {} 1", "a, ".repeat(200_000));
        let started = std::time::Instant::now();
        let c = pg(&sql);
        assert!(c.pageable);
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        let script = "SELECT 1;".repeat(50_000);
        assert_eq!(pg(&script).statements.len(), 50_000);
    }

    // ------------------------------------------------ re-review regressions

    #[test]
    fn dml_in_nested_with_is_a_write() {
        for sql in [
            "WITH x AS (WITH y AS (SELECT 1) DELETE FROM t RETURNING *) SELECT * FROM x",
            "WITH x AS (WITH y AS (SELECT 1) DELETE FROM t WHERE id IN (SELECT * FROM y) RETURNING *) SELECT * FROM x",
            "WITH x AS (WITH y AS (SELECT 1) UPDATE t SET a = 1 RETURNING *) SELECT * FROM x",
            "WITH x AS (WITH y AS (SELECT 1) INSERT INTO t SELECT * FROM y RETURNING *) SELECT * FROM x",
            "WITH x AS (WITH y AS (SELECT 1) MERGE INTO t USING y ON true WHEN MATCHED THEN UPDATE SET a = 1 RETURNING *) SELECT * FROM x",
            "WITH a AS (WITH b AS (WITH c AS (SELECT 1) DELETE FROM t RETURNING *) SELECT * FROM b) SELECT * FROM a",
            "WITH a AS (SELECT 1), x AS (WITH y AS (SELECT 1) INSERT INTO t VALUES (1) RETURNING *) SELECT * FROM x",
            "WITH x AS (SELECT 1) INSERT INTO t SELECT * FROM x RETURNING *",
            "WITH x AS (SELECT 1) INSERT INTO t SELECT * FROM x",
            "WITH x AS (SELECT 1) UPDATE t SET a = 1 RETURNING *",
            "INSERT INTO t SELECT * FROM y RETURNING *",
        ] {
            assert!(!pg(sql).pageable, "{sql:?}");
        }
        // Dangerous when the nested DELETE/UPDATE has no WHERE of its own: a
        // WHERE somewhere else (an inner SELECT, the outer query) does not protect it.
        for sql in [
            "WITH x AS (WITH y AS (SELECT 1) DELETE FROM t RETURNING *) SELECT * FROM x",
            "WITH x AS (WITH y AS (SELECT 1 WHERE true) DELETE FROM t RETURNING *) SELECT * FROM x WHERE true",
            "WITH x AS (WITH y AS (SELECT 1) UPDATE t SET a = 1 RETURNING *) SELECT * FROM x",
            "WITH a AS (WITH b AS (WITH c AS (SELECT 1) DELETE FROM t RETURNING *) SELECT * FROM b) SELECT * FROM a",
        ] {
            assert!(pg(sql).danger.is_some(), "{sql:?}");
        }
        for sql in [
            "WITH x AS (WITH y AS (SELECT 1) DELETE FROM t WHERE id = 1 RETURNING *) SELECT * FROM x",
            "WITH x AS (WITH y AS (SELECT 1) UPDATE t SET a = 1 WHERE id = 1 RETURNING *) SELECT * FROM x",
        ] {
            assert_eq!(danger(&pg(sql)), None, "{sql:?}");
        }
        // A nested recursive CTE with SEARCH / CYCLE puts plain words before the DML.
        for sql in [
            "WITH x AS (WITH RECURSIVE y(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM y WHERE n<3) SEARCH DEPTH FIRST BY n SET ord DELETE FROM t RETURNING *) SELECT * FROM x",
            "WITH x AS (WITH RECURSIVE y(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM y WHERE n<3) CYCLE n SET is_cycle USING path DELETE FROM t RETURNING *) SELECT * FROM x",
            "WITH x AS (WITH RECURSIVE y(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM y WHERE n<3) SEARCH BREADTH FIRST BY n SET ord UPDATE t SET a = 1 RETURNING *) SELECT * FROM x",
            "WITH x AS (WITH RECURSIVE y(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM y WHERE n<3) SEARCH DEPTH FIRST BY n SET ord INSERT INTO t (a) VALUES (1) RETURNING *) SELECT * FROM x",
            "WITH x AS (WITH RECURSIVE y(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM y WHERE n<3) CYCLE n SET is_cycle USING path INSERT INTO t SELECT * FROM y RETURNING *) SELECT * FROM x",
            "WITH x AS (WITH RECURSIVE y(n) AS (SELECT 1) CYCLE n SET c USING p MERGE INTO t USING y ON true WHEN MATCHED THEN UPDATE SET a = 1 RETURNING *) SELECT * FROM x",
            "WITH a AS (WITH b AS (WITH RECURSIVE y(n) AS (SELECT 1) SEARCH DEPTH FIRST BY n SET ord DELETE FROM t RETURNING *) SELECT * FROM b) SELECT * FROM a",
        ] {
            assert!(!pg(sql).pageable, "{sql:?}");
        }
        for sql in [
            "WITH x AS (WITH RECURSIVE y(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM y WHERE n<3) SEARCH DEPTH FIRST BY n SET ord DELETE FROM t RETURNING *) SELECT * FROM x",
            "WITH x AS (WITH RECURSIVE y(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM y WHERE n<3) CYCLE n SET is_cycle USING path DELETE FROM t RETURNING *) SELECT * FROM x",
            "WITH x AS (WITH RECURSIVE y(n) AS (SELECT 1) SEARCH BREADTH FIRST BY n SET ord UPDATE t SET a = 1 RETURNING *) SELECT * FROM x",
            "WITH a AS (WITH b AS (WITH RECURSIVE y(n) AS (SELECT 1) SEARCH DEPTH FIRST BY n SET ord DELETE FROM t RETURNING *) SELECT * FROM b) SELECT * FROM a",
        ] {
            assert!(pg(sql).danger.is_some(), "{sql:?}");
        }
        for sql in [
            "WITH x AS (WITH RECURSIVE y(n) AS (SELECT 1) SEARCH DEPTH FIRST BY n SET ord DELETE FROM t WHERE n = 1 RETURNING *) SELECT * FROM x",
            "WITH x AS (WITH RECURSIVE y(n) AS (SELECT 1) CYCLE n SET c USING p UPDATE t SET a = 1 WHERE id = 1 RETURNING *) SELECT * FROM x",
            "WITH x AS (WITH RECURSIVE y(n) AS (SELECT 1) CYCLE n SET c USING p INSERT INTO t (a) VALUES (1) RETURNING *) SELECT * FROM x",
            // `DO UPDATE` of an upsert is not an UPDATE statement.
            "WITH x AS (WITH y AS (SELECT 1) SEARCH DEPTH FIRST BY n SET ord INSERT INTO t (a) VALUES (1) ON CONFLICT (a) DO UPDATE SET a = 2 RETURNING *) SELECT * FROM x",
        ] {
            assert_eq!(danger(&pg(sql)), None, "{sql:?}");
        }
        // Lock clauses and string functions after a nested WITH are not writes.
        for sql in [
            "WITH x AS (WITH RECURSIVE y(n) AS (SELECT 1) SEARCH DEPTH FIRST BY n SET ord SELECT * FROM y FOR UPDATE) SELECT * FROM x",
            "WITH x AS (WITH y AS (SELECT 1) SELECT * FROM y FOR NO KEY UPDATE SKIP LOCKED) SELECT * FROM x",
            "WITH x AS (WITH y AS (SELECT 1) SELECT * FROM y FOR SHARE) SELECT * FROM x",
            "WITH x AS (WITH y AS (SELECT 'a' AS c) SELECT replace(c, 'a', 'b'), insert(c, 1, 1, 'z') FROM y) SELECT * FROM x",
        ] {
            let c = pg(sql);
            assert!(c.pageable, "{sql:?}");
            assert_eq!(c.danger, None, "{sql:?}");
        }
        // The bit is per depth: a sibling body without a nested WITH is unaffected.
        assert!(pg("WITH x AS (WITH y AS (SELECT 1) SELECT * FROM y), z AS (SELECT * FROM t FOR UPDATE) SELECT * FROM x, z").pageable);
        // Nested reads stay pageable.
        for sql in [
            "WITH x AS (WITH y AS (SELECT 1) SELECT * FROM y) SELECT * FROM x",
            "WITH x AS (SELECT * FROM (SELECT 1) s FOR UPDATE) SELECT * FROM x",
            "WITH x AS (SELECT (1) AS update_flag, (2) delete_flag) SELECT * FROM x",
            "WITH x AS ((SELECT 1) UNION (SELECT 2)) SELECT * FROM x",
        ] {
            assert!(pg(sql).pageable, "{sql:?}");
        }
    }

    #[test]
    fn into_at_any_depth_or_on_values_and_table_is_not_a_read() {
        for sql in [
            "(SELECT 1 AS a INTO new_t)",
            "SELECT * FROM (SELECT 1 AS a INTO new_t) s",
            "SELECT 1 INTO new_t",
            "SELECT * INTO new_t FROM t",
            "WITH x AS (SELECT 1) SELECT * INTO new_t FROM x",
            "WITH x AS (SELECT 1) SELECT * FROM (SELECT 2 INTO y) z",
        ] {
            assert!(!pg(sql).pageable, "pg {sql:?}");
            assert!(!my(sql).pageable, "my {sql:?}");
        }
        for sql in [
            "TABLE t INTO OUTFILE '/tmp/x'",
            "VALUES ROW(1) INTO OUTFILE '/tmp/x'",
            "SELECT 1 INTO OUTFILE '/tmp/x'",
            "SELECT 1 INTO @v",
            "WITH x AS (SELECT 1) TABLE x INTO DUMPFILE '/tmp/x'",
        ] {
            assert!(!my(sql).pageable, "my {sql:?}");
        }
        for sql in [
            "TABLE t",
            "VALUES ROW(1)",
            "VALUES (1)",
            "SELECT 'into'",
            "SELECT \"into\" FROM t",
        ] {
            assert!(my(sql).pageable || sql.starts_with("SELECT \""), "{sql:?}");
        }
        assert!(pg("SELECT \"into\" FROM t").pageable);
        assert!(my("SELECT `into` FROM t").pageable);
    }

    #[test]
    fn describe_and_explain_analyze_run_the_statement() {
        for sql in [
            "DESC ANALYZE SELECT 1",
            "DESCRIBE ANALYZE SELECT * FROM t",
            "desc analyze format=json select 1",
            "EXPLAIN ANALYZE SELECT 1",
            "EXPLAIN ANALYSE SELECT 1",
            "EXPLAIN (ANALYZE) DELETE FROM t WHERE id = 1",
            "ANALYZE SELECT 1",
        ] {
            assert!(!my(sql).pageable, "{sql:?}");
            assert!(!pg(sql).pageable, "{sql:?}");
        }
        for sql in [
            "DESC t",
            "DESCRIBE t",
            "EXPLAIN SELECT 1",
            "EXPLAIN FORMAT=JSON SELECT 1",
        ] {
            assert!(my(sql).pageable, "{sql:?}");
        }
    }

    #[test]
    fn sqlite_bracket_identifiers_are_inert_and_do_not_split_statements() {
        // The apostrophes live inside bracket identifiers: they are not strings.
        let c = lite("SELECT [x'] ; DELETE FROM t; SELECT ['] ; SELECT 1");
        assert_eq!(c.statements.len(), 4, "{c:?}");
        assert_eq!(c.danger.map(|d| d.statement), Some(1));
        assert!(!c.pageable);
        // `;` inside a bracket identifier does not split.
        let c = lite("SELECT [a;b] FROM [t;u]");
        assert_eq!(c.statements.len(), 1);
        assert!(c.pageable);
        // `]]` is an escaped bracket.
        assert_eq!(lite("SELECT [a]];DROP TABLE t] FROM t").statements.len(), 1);
        // A bracket identifier hides keywords.
        assert!(lite("SELECT [DROP TABLE t; DELETE FROM t]").pageable);
        assert_eq!(
            lite("SELECT [unterminated; DROP TABLE t").statements.len(),
            1
        );
        // Other dialects do not treat `[` specially (PostgreSQL array subscripts).
        let c = pg("SELECT a[1]; DROP TABLE t");
        assert_eq!(c.statements.len(), 2);
        assert!(c.danger.is_some());
        assert_eq!(my("SELECT a[1]; DROP TABLE t").statements.len(), 2);
    }

    #[test]
    fn merge_that_deletes_unconditionally_is_dangerous() {
        let delete = "MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN DELETE";
        let c = pg(delete);
        assert!(!c.pageable);
        assert!(danger(&c).unwrap().contains("MERGE"), "{c:?}");
        assert!(
            pg("merge into t using s on t.id = s.id when matched then delete")
                .danger
                .is_some()
        );
        // An AND condition on the WHEN clause makes the delete conditional.
        for sql in [
            "MERGE INTO t USING s ON t.id = s.id WHEN MATCHED AND s.gone THEN DELETE",
            "MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN UPDATE SET a = 1",
            "MERGE INTO t USING s ON t.id = s.id WHEN NOT MATCHED THEN INSERT (id) VALUES (s.id)",
            "MERGE INTO t USING s ON t.id = s.id WHEN MATCHED AND s.a THEN UPDATE SET a = 1 WHEN MATCHED AND s.b THEN DELETE",
        ] {
            let c = pg(sql);
            assert!(!c.pageable, "{sql:?}");
            assert_eq!(c.danger, None, "{sql:?}");
        }
        // Each WHEN clause is judged on its own; ANDs in ON do not count.
        assert!(
            pg("MERGE INTO t USING s ON t.id = s.id AND t.x = s.x WHEN MATCHED THEN DELETE")
                .danger
                .is_some()
        );
        assert!(pg("MERGE INTO t USING s ON t.id = s.id WHEN MATCHED AND s.a THEN UPDATE SET a = 1 WHEN MATCHED THEN DELETE").danger.is_some());
        // PostgreSQL 17 source-side delete.
        assert!(
            pg("MERGE INTO t USING s ON t.id = s.id WHEN NOT MATCHED BY SOURCE THEN DELETE")
                .danger
                .is_some()
        );
        // Inside a CTE.
        let c = pg("WITH x AS (MERGE INTO t USING s ON true WHEN MATCHED THEN DELETE RETURNING *) SELECT * FROM x");
        assert!(!c.pageable && c.danger.is_some());
        // THEN DELETE without MERGE is not a thing the classifier flags.
        assert_eq!(danger(&pg("SELECT 'WHEN MATCHED THEN DELETE'")), None);
    }

    #[test]
    fn blind_spots_are_documented_not_detected() {
        // Procedural code and side-effecting functions are NOT seen (documented).
        assert!(pg("SELECT nextval('s')").pageable);
        assert!(
            !pg("DO $$ BEGIN DELETE FROM t; END $$").pageable,
            "DO is not a read"
        );
        assert_eq!(pg("DO $$ BEGIN DELETE FROM t; END $$").danger, None);
        assert_eq!(my("CALL p()").danger, None);
    }

    fn ch(sql: &str) -> Classification {
        classify(sql, &SqlLexOptions::clickhouse())
    }

    #[test]
    fn clickhouse_reads_its_quotes_comments_and_heredocs() {
        // Backslash escapes inside strings and both identifier quotes.
        assert!(ch(r"SELECT 'it\'s; DROP TABLE x'").pageable);
        assert_eq!(danger(&ch(r"SELECT 'it\'s; DROP TABLE x'")), None);
        assert_eq!(danger(&ch(r#"SELECT "a\"; DROP TABLE x" FROM t"#)), None);
        assert_eq!(danger(&ch(r"SELECT `a\`; DROP TABLE x` FROM t")), None);
        // `--`, `#` and `/* */` comments hide keywords; comments do not nest.
        assert_eq!(danger(&ch("SELECT 1 -- DROP TABLE x\n")), None);
        assert_eq!(danger(&ch("SELECT 1 # DROP TABLE x\n")), None);
        assert_eq!(danger(&ch("SELECT 1 /* DROP TABLE x */")), None);
        assert!(danger(&ch("SELECT 1 /* a /* b */ ; DROP TABLE x")).is_some());
        // `$$ … $$` heredoc strings.
        assert_eq!(danger(&ch("SELECT $$; DROP TABLE x$$")), None);
        // Other dialects keep their identifier rules: the backslash is literal.
        assert_eq!(
            danger(&my(r"SELECT `a\`; DROP TABLE x")),
            Some("DROP permanently removes database objects")
        );
    }

    #[test]
    fn clickhouse_dangers_and_pageable_reads() {
        assert!(ch("SELECT 1; SELECT 2").statements.len() == 2);
        assert!(ch("DESCRIBE TABLE t").pageable && ch("SHOW TABLES").pageable);
        assert!(ch("EXPLAIN SELECT 1").pageable);
        assert!(danger(&ch("ALTER TABLE t DELETE WHERE id = 1")).is_some());
        assert!(danger(&ch("ALTER TABLE t UPDATE a = 1 WHERE id = 1")).is_some());
        assert!(danger(&ch("DROP TABLE t")).is_some());
        assert!(danger(&ch("TRUNCATE TABLE t")).is_some());
        assert_eq!(danger(&ch("DELETE FROM t WHERE id = 1")), None);
        assert!(danger(&ch("DELETE FROM t")).is_some());
        assert!(!ch("INSERT INTO t VALUES (1)").pageable);
    }

    /// `(kind, text)` of every token of `sql`.
    fn scan<'a>(sql: &'a str, options: &SqlLexOptions) -> Vec<(SqlTokenKind, &'a str)> {
        tokens(sql, options)
            .tokens
            .into_iter()
            .map(|token| (token.kind, &sql[token.range]))
            .collect()
    }

    #[test]
    fn tokens_skip_comments_and_keep_literals_whole() {
        use SqlTokenKind::{Close, Comma, Number, Open, Other, Quoted, Semicolon, Word};
        let sql = "SELECT * /* c; */ FROM \"a;b\" -- x\n WHERE n >= 1.5 AND s = 'it''s' ; (1, 2)";
        assert_eq!(
            scan(sql, &SqlLexOptions::postgres()),
            [
                (Word, "SELECT"),
                (Other, "*"),
                (Word, "FROM"),
                (Quoted, "\"a;b\""),
                (Word, "WHERE"),
                (Word, "n"),
                (Other, ">"),
                (Other, "="),
                (Number, "1.5"),
                (Word, "AND"),
                (Word, "s"),
                (Other, "="),
                (Quoted, "'it''s'"),
                (Semicolon, ";"),
                (Open, "("),
                (Number, "1"),
                (Comma, ","),
                (Number, "2"),
                (Close, ")"),
            ]
        );
    }

    #[test]
    fn tokens_follow_the_dialect_options() {
        use SqlTokenKind::{Quoted, Word};
        // A backslash escapes in MySQL strings only.
        let sql = r"SELECT 'a\' x";
        assert_eq!(
            scan(sql, &SqlLexOptions::mysql()),
            [(Word, "SELECT"), (Quoted, r"'a\' x")]
        );
        assert!(tokens(sql, &SqlLexOptions::mysql()).unterminated);
        assert_eq!(
            scan(sql, &SqlLexOptions::mysql().without_backslash_escapes()),
            [(Word, "SELECT"), (Quoted, r"'a\'"), (Word, "x")]
        );
        assert_eq!(scan(sql, &SqlLexOptions::postgres()).len(), 3);
        // `#` starts a comment in MySQL only.
        assert_eq!(scan("a # b\nc", &SqlLexOptions::mysql()).len(), 2);
        assert_eq!(scan("a # b\nc", &SqlLexOptions::postgres()).len(), 4);
    }

    #[test]
    fn tokens_flag_unterminated_text() {
        let pg = SqlLexOptions::postgres();
        assert!(!tokens("SELECT 'a' /* b */ \"c\"", &pg).unterminated);
        for sql in ["SELECT 'a", "SELECT \"a", "SELECT 1 /* a", "SELECT $x$ a", "SELECT 'a''"] {
            assert!(tokens(sql, &pg).unterminated, "{sql}");
        }
        // A nested comment is closed only by its own `*/` on PostgreSQL.
        assert!(tokens("a /* b /* c */", &pg).unterminated);
        assert!(!tokens("a /* b /* c */", &SqlLexOptions::mysql()).unterminated);
    }

    #[test]
    fn tokens_flag_executable_comments_the_server_runs() {
        let my = SqlLexOptions::mysql();
        assert!(!tokens("SELECT 1 /* plain */", &my).executable_comment);
        let runs = tokens("SELECT /*!50000 1 */", &my.with_server_version(80000));
        assert!(runs.executable_comment);
        assert_eq!(runs.tokens.len(), 2);
        // Above the server's version it is a plain comment.
        let skipped = tokens("SELECT /*!99999 1 */", &my.with_server_version(80000));
        assert!(!skipped.executable_comment);
        assert_eq!(skipped.tokens.len(), 1);
        // PostgreSQL has no executable comments.
        assert!(!tokens("SELECT /*!50000 1 */", &SqlLexOptions::postgres()).executable_comment);
    }

    #[test]
    fn a_carriage_return_ends_a_line_comment_on_postgres_only() {
        let sql = "SELECT 1; SELECT 2 --x\r; DELETE FROM t";
        // PostgreSQL: the comment ends at `\r`, so the `;` is a real separator.
        let pg = classify(sql, &SqlLexOptions::postgres());
        assert_eq!(pg.statements.len(), 3);
        assert_eq!(
            pg.danger.map(|danger| danger.reason),
            Some("DELETE without WHERE removes every row of the table")
        );
        // The others read it to `\n`: the rest of the line is the comment.
        for options in [
            SqlLexOptions::mysql(),
            SqlLexOptions::mariadb(),
            SqlLexOptions::sqlite(),
            SqlLexOptions::clickhouse(),
        ] {
            let classified = classify("SELECT 1; SELECT 2 -- x\r; DELETE FROM t", &options);
            assert_eq!(classified.statements.len(), 2, "{options:?}");
            assert!(classified.danger.is_none(), "{options:?}");
        }
        // `\n` ends it everywhere.
        for options in [SqlLexOptions::postgres(), SqlLexOptions::mysql(), SqlLexOptions::sqlite()] {
            assert!(classify("SELECT 2 -- x\n; DELETE FROM t", &options).danger.is_some());
        }
        assert!(
            classify("SELECT 2 # x\r; DELETE FROM t", &SqlLexOptions::mysql()).danger.is_none()
        );
    }

    #[test]
    fn tokens_end_a_line_comment_at_a_carriage_return_on_postgres_only() {
        let sql = "a -- x\rb";
        assert_eq!(scan(sql, &SqlLexOptions::postgres()).len(), 2);
        assert_eq!(scan(sql, &SqlLexOptions::sqlite()).len(), 1);
        assert_eq!(scan("a # x\rb", &SqlLexOptions::mysql()).len(), 1);
    }
}
