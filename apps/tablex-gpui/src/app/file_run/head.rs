//! What a statement is, told from its first words: whether it begins or ends a transaction, whether the server
//! commits by itself before it, whether it only changes the session (so a resumed run must repeat it), and which
//! database a `USE` selects. A lexer for the first few words, not a parser: comments and blanks are skipped, a
//! MySQL / MariaDB versioned comment (`/*!40101 SET … */`, `/*M!100000 … */`) is read as the code it is when the
//! server runs it, and nothing is allocated for a word.

/// How MySQL and MariaDB read `/*! … */`: its content is code when the server's version is not below the
/// comment's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) struct Comments {
    /// The server's version in MySQL's numeric form (8.4.6 = `80406`); `None`: unknown, so a versioned comment is code.
    pub(in crate::app) version: Option<u32>,
    pub(in crate::app) mariadb: bool,
    /// `sql_mode` has `ANSI_QUOTES`: `"b"` is a name.
    pub(in crate::app) ansi_quotes: bool,
}

impl Comments {
    /// Whether the server runs the versioned comment whose digits are `digits` (`mariadb_form`: `/*M!`).
    fn runs(self, mariadb_form: bool, digits: &[u8]) -> bool {
        // Fewer than five digits are not a version: they are part of the code. Seven or more are read differently by
        // the two servers: read as code.
        if digits.len() < 5 || digits.len() > 6 {
            return true;
        }
        let Some(server) = self.version else { return true };
        let value = digits.iter().fold(0_u32, |value, digit| value * 10 + u32::from(digit - b'0'));
        // MariaDB skips the MySQL-only versions 50700 to 99999 of `/*!`.
        let mysql_only = self.mariadb && !mariadb_form && (50_700..100_000).contains(&value);
        value <= server && !mysql_only
    }
}

/// The words at the start of a statement.
pub(in crate::app) struct Words<'a> {
    rest: &'a [u8],
    comments: Option<Comments>,
    /// Inside a versioned comment the server runs: its `*/` is blank.
    in_code_comment: bool,
}

impl<'a> Words<'a> {
    pub(in crate::app) fn new(text: &'a [u8], comments: Option<Comments>) -> Self {
        Self { rest: text, comments, in_code_comment: false }
    }

    /// Skips blanks, comments and the delimiters of a versioned comment that is code.
    fn skip_blank(&mut self) {
        loop {
            let blanks = self.rest.iter().take_while(|byte| byte.is_ascii_whitespace()).count();
            self.rest = &self.rest[blanks..];
            if self.rest.starts_with(b"--") || (self.comments.is_some() && self.rest.starts_with(b"#")) {
                self.rest = &self.rest[self.rest.iter().position(|&byte| byte == b'\n').unwrap_or(self.rest.len())..];
            } else if self.in_code_comment && self.rest.starts_with(b"*/") {
                self.in_code_comment = false;
                self.rest = &self.rest[2..];
            } else if self.rest.starts_with(b"/*") {
                let mariadb_form = self.rest.starts_with(b"/*M!");
                let versioned = self.comments.filter(|comments| !self.in_code_comment && (self.rest.starts_with(b"/*!") || (mariadb_form && comments.mariadb)));
                match versioned {
                    Some(comments) => {
                        let prefix = if mariadb_form { 4 } else { 3 };
                        let digits = self.rest[prefix..].iter().take_while(|byte| byte.is_ascii_digit()).count();
                        if comments.runs(mariadb_form, &self.rest[prefix..prefix + digits]) {
                            // With fewer than five digits they belong to the code (`/*!40 …` is not a version).
                            let version_digits = if digits < 5 { 0 } else { digits };
                            self.rest = &self.rest[prefix + version_digits..];
                            self.in_code_comment = true;
                        } else {
                            self.rest = self.after_block_comment();
                        }
                    }
                    None => self.rest = self.after_block_comment(),
                }
            } else {
                return;
            }
        }
    }

    fn after_block_comment(&self) -> &'a [u8] {
        let end = self.rest.windows(2).position(|pair| pair == b"*/").map_or(self.rest.len(), |at| at + 2);
        &self.rest[end..]
    }

    /// The next word (letters only), or `None` at anything else.
    pub(in crate::app) fn word(&mut self) -> Option<&'a [u8]> {
        self.skip_blank();
        let length = self.rest.iter().take_while(|byte| byte.is_ascii_alphabetic()).count();
        let (word, rest) = self.rest.split_at(length);
        self.rest = rest;
        (!word.is_empty()).then_some(word)
    }

    /// The next word when it is `expected` (any case); nothing is consumed otherwise.
    fn word_is(&mut self, expected: &[u8]) -> bool {
        let saved = (self.rest, self.in_code_comment);
        if self.word().is_some_and(|word| word.eq_ignore_ascii_case(expected)) {
            return true;
        }
        (self.rest, self.in_code_comment) = saved;
        false
    }

    /// What follows the words read so far, blanks and comments skipped.
    fn rest(&mut self) -> &'a [u8] {
        self.skip_blank();
        self.rest
    }

    /// A name: `` `quoted` `` (or `"quoted"` under `ANSI_QUOTES`), doubled quote characters inside, or a run up to a blank, `;`, a
    /// comma or the start of a comment.
    pub(in crate::app) fn identifier(&mut self) -> Option<String> {
        self.skip_blank();
        let quote = match self.rest.first() {
            Some(b'`') => Some(b'`'),
            Some(b'"') if self.comments.is_some_and(|comments| comments.ansi_quotes) => Some(b'"'),
            _ => None,
        };
        if let Some(quote) = quote {
            let quoted = &self.rest[1..];
            let mut name = Vec::new();
            let mut at = 0;
            while at < quoted.len() {
                if quoted[at] == quote {
                    if quoted.get(at + 1) == Some(&quote) {
                        name.push(quote);
                        at += 2;
                        continue;
                    }
                    return String::from_utf8(name).ok().filter(|name| !name.is_empty());
                }
                name.push(quoted[at]);
                at += 1;
            }
            return None;
        }
        let mut length = 0;
        while let Some(&byte) = self.rest.get(length) {
            let comment_starts = self.rest[length..].starts_with(b"/*") || self.rest[length..].starts_with(b"--") || byte == b'#' || self.rest[length..].starts_with(b"*/");
            if byte.is_ascii_whitespace() || matches!(byte, b';' | b'`' | b'\'' | b'"' | b',') || comment_starts {
                break;
            }
            length += 1;
        }
        let name = std::str::from_utf8(&self.rest[..length]).ok()?;
        (!name.is_empty()).then(|| name.to_owned())
    }
}

fn is(word: Option<&[u8]>, expected: &[u8]) -> bool {
    word.is_some_and(|word| word.eq_ignore_ascii_case(expected))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum Transaction {
    Begin,
    End,
    /// `COMMIT AND CHAIN`, `ROLLBACK AND CHAIN`: one transaction ends and the next begins at once.
    Chain,
    /// MySQL and MariaDB: `SET autocommit = 0` makes the next statement begin a transaction, and every one after a commit.
    AutocommitOff,
    AutocommitOn,
}

fn starts_with_ignore_case(bytes: &[u8], prefix: &[u8]) -> bool {
    bytes.len() >= prefix.len() && bytes[..prefix.len()].eq_ignore_ascii_case(prefix)
}

/// What `SET autocommit = value` sets (`Some(true)`: on), after `SET`. `rest` starts at the name.
fn autocommit_value(rest: &[u8]) -> Option<bool> {
    let mut rest = rest;
    let skip_blank = |rest: &mut &[u8]| *rest = &rest[rest.iter().take_while(|byte| byte.is_ascii_whitespace()).count()..];
    let strip = |rest: &mut &[u8], prefix: &[u8]| {
        let found = starts_with_ignore_case(rest, prefix);
        if found {
            *rest = &rest[prefix.len()..];
        }
        found
    };
    if strip(&mut rest, b"@@") {
        let _ = strip(&mut rest, b"session.") || strip(&mut rest, b"local.");
    } else if [&b"session"[..], b"local"].iter().any(|scope| starts_with_ignore_case(rest, scope) && rest.get(scope.len()).is_some_and(u8::is_ascii_whitespace)) {
        rest = &rest[rest.iter().position(u8::is_ascii_whitespace).unwrap_or(rest.len())..];
        skip_blank(&mut rest);
    }
    if !starts_with_ignore_case(rest, b"autocommit") {
        return None;
    }
    rest = &rest[10..];
    if rest.first().is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_') {
        return None;
    }
    skip_blank(&mut rest);
    strip(&mut rest, b":");
    if !strip(&mut rest, b"=") {
        return None;
    }
    skip_blank(&mut rest);
    let length = rest.iter().take_while(|byte| byte.is_ascii_alphanumeric()).count();
    let value = &rest[..length];
    if value == b"0" || value.eq_ignore_ascii_case(b"off") || value.eq_ignore_ascii_case(b"false") {
        Some(false)
    } else if value == b"1" || value.eq_ignore_ascii_case(b"on") || value.eq_ignore_ascii_case(b"true") {
        Some(true)
    } else {
        None
    }
}

/// Whether the statement begins or ends a transaction: `BEGIN` (not MariaDB's `BEGIN NOT ATOMIC` compound statement),
/// `START TRANSACTION`, `COMMIT`, `END`, `ROLLBACK`, `ABORT`, PostgreSQL's `PREPARE TRANSACTION` (which ends it) and, on MySQL and
/// MariaDB, `SET autocommit`; `… AND CHAIN` ends one and begins the next; not a rollback to a savepoint, nor `COMMIT PREPARED` /
/// `ROLLBACK PREPARED`.
pub(in crate::app) fn transaction_of(text: &[u8], comments: Option<Comments>) -> Option<Transaction> {
    let mut words = Words::new(text, comments);
    let first = words.word()?;
    let lower = |expected: &[u8]| first.eq_ignore_ascii_case(expected);
    if lower(b"begin") {
        // `BEGIN NOT ATOMIC … END` is a compound statement, not a transaction.
        return (!(words.word_is(b"not") && words.word_is(b"atomic"))).then_some(Transaction::Begin);
    }
    if lower(b"start") {
        return words.word_is(b"transaction").then_some(Transaction::Begin);
    }
    if lower(b"prepare") {
        return words.word_is(b"transaction").then_some(Transaction::End);
    }
    if lower(b"set") && comments.is_some() {
        return autocommit_value(words.rest()).map(|on| if on { Transaction::AutocommitOn } else { Transaction::AutocommitOff });
    }
    if lower(b"commit") || lower(b"end") || lower(b"rollback") || lower(b"abort") {
        let mut next = words.word();
        if is(next, b"work") || is(next, b"transaction") || is(next, b"tran") {
            next = words.word();
        }
        if is(next, b"and") {
            return Some(if words.word_is(b"chain") { Transaction::Chain } else { Transaction::End });
        }
        return (!(is(next, b"to") || is(next, b"prepared") || is(next, b"savepoint"))).then_some(Transaction::End);
    }
    None
}

/// Whether MySQL and MariaDB commit the open transaction before running the statement (DDL, account statements, table
/// maintenance, replication control, `LOCK TABLES`), also when the statement sits in a versioned comment
/// (`/*!40000 ALTER TABLE t DISABLE KEYS */`). A `TEMPORARY` table's `CREATE` and `DROP` (also `CREATE OR REPLACE TEMPORARY`) do
/// not. A false answer costs the run one COMMIT it did not need; the run sends it itself before such a statement, so what it
/// believes about its transaction is never different from the server's.
pub(in crate::app) fn commits_implicitly(text: &[u8], comments: Comments) -> bool {
    let mut words = Words::new(text, Some(comments));
    let Some(first) = words.word() else { return false };
    let lower = |expected: &[u8]| first.eq_ignore_ascii_case(expected);
    if lower(b"create") || lower(b"drop") {
        if words.word_is(b"or") {
            words.word_is(b"replace");
        }
        return !words.word_is(b"temporary");
    }
    if lower(b"start") || lower(b"stop") {
        return words.word_is(b"replica") || words.word_is(b"slave") || words.word_is(b"group_replication");
    }
    if lower(b"change") {
        return words.word_is(b"replication") || words.word_is(b"master");
    }
    if lower(b"set") {
        return words.word_is(b"password");
    }
    // `LOAD INDEX INTO CACHE` commits; `LOAD DATA` does not (but on NDB tables).
    if lower(b"load") {
        return words.word_is(b"index");
    }
    [&b"import"[..], b"alter", b"rename", b"truncate", b"grant", b"revoke", b"analyze", b"optimize", b"repair", b"check", b"cache", b"flush", b"reset", b"lock", b"unlock", b"install", b"uninstall"].iter().any(|word| lower(word))
}

/// What a run that starts at statement N does with a statement before N.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum SessionStatement {
    /// It only changes the session (its database, its settings, with values that are literals or variables): it is run again,
    /// so N runs where it would have in the whole run.
    Repeat,
    /// A statement that sets something and is not run again: the reason is listed in the summary.
    NotRepeated(&'static str),
    Other,
}

pub(in crate::app) const NOT_REPEATED_SERVER: &str = "it changes the server, not the session";
pub(in crate::app) const NOT_REPEATED_TRANSACTION: &str = "it applies to a transaction, not to the session";
pub(in crate::app) const NOT_REPEATED_ACCOUNT: &str = "it changes an account or a role";
pub(in crate::app) const NOT_REPEATED_AUTOCOMMIT: &str = "it changes how transactions end: the run does not carry that over";
pub(in crate::app) const NOT_REPEATED_COMPUTED: &str = "its value is computed (a function or an expression), which could differ the second time";

/// Values that look like a word or a variable but are computed when read: a fresh session gives another one.
const COMPUTED_WORDS: [&[u8]; 16] = [
    b"@@identity",
    b"@@last_insert_id",
    b"@@insert_id",
    b"@@timestamp",
    b"@@warning_count",
    b"@@error_count",
    b"current_timestamp",
    b"current_date",
    b"current_time",
    b"current_user",
    b"current_role",
    b"localtime",
    b"localtimestamp",
    b"utc_date",
    b"utc_time",
    b"utc_timestamp",
];

#[derive(Debug, PartialEq, Eq)]
enum Values {
    Simple,
    Computed,
    Global,
}

/// Whether what follows `SET name =` is only literals, variables and words, and no `GLOBAL` / `PERSIST` scope is named. `in_call`:
/// stop at the `)` that closes a call (`set_config(…)`); `code_comment`: stop at the `*/` that closes the versioned comment the
/// statement sits in.
fn scan_values(text: &[u8], mysql: bool, code_comment: bool, in_call: bool) -> Values {
    let mut rest = text;
    while let Some(&byte) = rest.first() {
        match byte {
            b'\'' | b'"' | b'`' => {
                let mut at = 1;
                loop {
                    match rest.get(at) {
                        None => return Values::Computed,
                        Some(b'\\') if mysql && byte != b'`' => at += 2,
                        Some(&found) if found == byte => {
                            if rest.get(at + 1) == Some(&byte) {
                                at += 2;
                            } else {
                                at += 1;
                                break;
                            }
                        }
                        Some(_) => at += 1,
                    }
                }
                rest = &rest[at.min(rest.len())..];
            }
            b'-' if rest.starts_with(b"--") => rest = &rest[rest.iter().position(|&found| found == b'\n').unwrap_or(rest.len())..],
            b'#' if mysql => rest = &rest[rest.iter().position(|&found| found == b'\n').unwrap_or(rest.len())..],
            b'/' if rest.starts_with(b"/*") => {
                // A versioned comment inside the value is code of its own: not read here.
                if rest.starts_with(b"/*!") || rest.starts_with(b"/*M!") {
                    return Values::Computed;
                }
                rest = &rest[rest.windows(2).position(|pair| pair == b"*/").map_or(rest.len(), |at| at + 2)..];
            }
            b'*' if code_comment && rest.starts_with(b"*/") => return Values::Simple,
            // The call is the whole statement: nothing but blanks and `;` after it.
            b')' if in_call => return if rest[1..].iter().all(|byte| byte.is_ascii_whitespace() || *byte == b';') { Values::Simple } else { Values::Computed },
            b';' => rest = &rest[1..],
            _ if byte.is_ascii_whitespace() || matches!(byte, b',' | b'=' | b':' | b'+' | b'-') => rest = &rest[1..],
            _ if byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$' | b'.' | b'@') || byte >= 0x80 => {
                let length = rest.iter().take_while(|found| found.is_ascii_alphanumeric() || matches!(found, b'_' | b'$' | b'.' | b'@') || **found >= 0x80).count();
                let token = &rest[..length];
                if token.eq_ignore_ascii_case(b"global") || token.eq_ignore_ascii_case(b"persist") || token.eq_ignore_ascii_case(b"persist_only") || starts_with_ignore_case(token, b"@@global.") || starts_with_ignore_case(token, b"@@persist") {
                    return Values::Global;
                }
                if COMPUTED_WORDS.iter().any(|word| token.eq_ignore_ascii_case(word)) {
                    return Values::Computed;
                }
                rest = &rest[length..];
            }
            _ => return Values::Computed,
        }
    }
    Values::Simple
}

/// The statements that set the session up: `USE`, `SET`, and pg_dump's `SELECT pg_catalog.set_config('search_path', '', false)`.
pub(in crate::app) fn session_statement(text: &[u8], comments: Option<Comments>) -> SessionStatement {
    let mut words = Words::new(text, comments);
    let Some(first) = words.word() else { return SessionStatement::Other };
    if first.eq_ignore_ascii_case(b"use") {
        return if comments.is_some() { SessionStatement::Repeat } else { SessionStatement::Other };
    }
    if first.eq_ignore_ascii_case(b"select") {
        // `select [pg_catalog.]set_config('…', '…', false)` as the first call, with arguments that are literals.
        let mut rest = words.rest();
        if comments.is_some() || text.len() > 1024 {
            return SessionStatement::Other;
        }
        if starts_with_ignore_case(rest, b"pg_catalog.") {
            rest = &rest[11..];
        }
        if !starts_with_ignore_case(rest, b"set_config") {
            return SessionStatement::Other;
        }
        let call = &rest[10..];
        let blanks = call.iter().take_while(|byte| byte.is_ascii_whitespace()).count();
        let Some(arguments) = call[blanks..].strip_prefix(b"(") else { return SessionStatement::Other };
        return if scan_values(arguments, false, false, true) == Values::Simple { SessionStatement::Repeat } else { SessionStatement::Other };
    }
    if !first.eq_ignore_ascii_case(b"set") {
        return SessionStatement::Other;
    }
    let mysql = comments.is_some();
    let rest = words.rest();
    if let Some(scoped) = rest.strip_prefix(b"@@") {
        let session_name = if starts_with_ignore_case(scoped, b"session.") { &scoped[8..] } else if starts_with_ignore_case(scoped, b"local.") { &scoped[6..] } else { scoped };
        if starts_with_ignore_case(scoped, b"global.") || starts_with_ignore_case(scoped, b"persist.") || starts_with_ignore_case(scoped, b"persist_only.") {
            return SessionStatement::NotRepeated(NOT_REPEATED_SERVER);
        }
        if autocommit_value(session_name).is_some() || starts_with_ignore_case(session_name, b"autocommit") {
            return SessionStatement::NotRepeated(NOT_REPEATED_AUTOCOMMIT);
        }
    } else if !rest.starts_with(b"@") {
        let mut look = Words::new(rest, comments);
        let name = look.word();
        let lower = |expected: &[u8]| is(name, expected);
        if lower(b"global") || lower(b"persist") || lower(b"persist_only") {
            return SessionStatement::NotRepeated(NOT_REPEATED_SERVER);
        }
        // `SET LOCAL` is for one transaction on PostgreSQL, the session scope on MySQL and MariaDB.
        if lower(b"transaction") || lower(b"constraints") || (lower(b"local") && !mysql) {
            return SessionStatement::NotRepeated(NOT_REPEATED_TRANSACTION);
        }
        if lower(b"password") || lower(b"role") || lower(b"default") {
            return SessionStatement::NotRepeated(NOT_REPEATED_ACCOUNT);
        }
        if lower(b"autocommit") || ((lower(b"session") || lower(b"local")) && look.word_is(b"autocommit")) {
            return SessionStatement::NotRepeated(NOT_REPEATED_AUTOCOMMIT);
        }
    }
    match scan_values(rest, mysql, words.in_code_comment, false) {
        Values::Simple => SessionStatement::Repeat,
        Values::Global => SessionStatement::NotRepeated(NOT_REPEATED_SERVER),
        Values::Computed => SessionStatement::NotRepeated(NOT_REPEATED_COMPUTED),
    }
}

/// The database a `USE name` selects.
pub(in crate::app) fn use_target(text: &[u8], comments: Comments) -> Option<String> {
    let mut words = Words::new(text, Some(comments));
    words.word_is(b"use").then(|| words.identifier()).flatten()
}

/// The database a `DROP DATABASE [IF EXISTS] name` (or `DROP SCHEMA`) drops.
pub(in crate::app) fn drop_database_target(text: &[u8], comments: Comments) -> Option<String> {
    let mut words = Words::new(text, Some(comments));
    if !words.word_is(b"drop") || !(words.word_is(b"database") || words.word_is(b"schema")) {
        return None;
    }
    if words.word_is(b"if") {
        words.word_is(b"exists");
    }
    words.identifier()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MYSQL: Comments = Comments { version: Some(80_036), mariadb: false, ansi_quotes: false };
    const MARIADB: Comments = Comments { version: Some(110_800), mariadb: true, ansi_quotes: false };
    const ANSI: Comments = Comments { version: Some(80_036), mariadb: false, ansi_quotes: true };

    #[test]
    fn transaction_statements_are_told_from_the_ones_that_only_look_like_them() {
        for (text, kind) in [
            ("begin", Transaction::Begin),
            ("BEGIN TRANSACTION", Transaction::Begin),
            ("  -- c\n/* d */ begin work", Transaction::Begin),
            ("START TRANSACTION", Transaction::Begin),
            ("commit", Transaction::End),
            ("COMMIT WORK", Transaction::End),
            ("COMMIT AND NO CHAIN", Transaction::End),
            ("end", Transaction::End),
            ("rollback", Transaction::End),
            ("ROLLBACK TRANSACTION", Transaction::End),
            ("abort", Transaction::End),
            ("PREPARE TRANSACTION 'x'", Transaction::End),
            ("COMMIT AND CHAIN", Transaction::Chain),
            ("ROLLBACK WORK AND CHAIN", Transaction::Chain),
        ] {
            assert_eq!(transaction_of(text.as_bytes(), None), Some(kind), "{text}");
        }
        for no in ["start slave", "rollback to savepoint s", "ROLLBACK WORK TO s", "commit prepared 'x'", "rollback prepared 'x'", "select 1", "/*!40101 SET x */", "", "-- begin\nselect 1", "beginning()", "BEGIN NOT ATOMIC SELECT 1; END", "prepare p as select 1", "set autocommit = 0"] {
            assert_eq!(transaction_of(no.as_bytes(), None), None, "{no}");
        }
        assert_eq!(transaction_of(b"BEGIN", Some(MARIADB)), Some(Transaction::Begin));
        assert_eq!(transaction_of(b"/*M!100100 BEGIN */", Some(MARIADB)), Some(Transaction::Begin));
    }

    #[test]
    fn set_autocommit_is_a_transaction_statement_on_mysql_and_mariadb() {
        for (text, kind) in [
            ("SET autocommit = 0", Transaction::AutocommitOff),
            ("set autocommit=OFF", Transaction::AutocommitOff),
            ("SET @@autocommit = 0", Transaction::AutocommitOff),
            ("SET SESSION autocommit = FALSE", Transaction::AutocommitOff),
            ("SET @@session.autocommit=0", Transaction::AutocommitOff),
            ("/*!40101 SET autocommit=0 */", Transaction::AutocommitOff),
            ("SET autocommit = 1", Transaction::AutocommitOn),
            ("SET @@autocommit := ON", Transaction::AutocommitOn),
        ] {
            assert_eq!(transaction_of(text.as_bytes(), Some(MYSQL)), Some(kind), "{text}");
        }
        for no in ["SET autocommit_x = 0", "SET names utf8", "SET @autocommit = 0", "SET autocommit = @x", "SET sql_mode = ''"] {
            assert_eq!(transaction_of(no.as_bytes(), Some(MYSQL)), None, "{no}");
        }
    }

    #[test]
    fn statements_the_server_commits_by_itself_are_told_also_inside_versioned_comments() {
        for yes in [
            "create table t(a)",
            "DROP INDEX i ON t",
            "-- c\nalter table t add b int",
            "TRUNCATE t",
            "lock tables t write",
            "create procedure p() begin end",
            "GRANT ALL ON *.* TO x",
            "optimize table t",
            "CHECK TABLE t",
            "CACHE INDEX t IN hot",
            "SET PASSWORD = 'x'",
            "START REPLICA",
            "stop slave",
            "CHANGE REPLICATION SOURCE TO SOURCE_HOST='h'",
            "CHANGE MASTER TO MASTER_HOST='h'",
            "CREATE OR REPLACE TABLE t (a int)",
            "/*!40000 ALTER TABLE `t` DISABLE KEYS */",
            "/*!50001 CREATE VIEW v AS SELECT 1 */",
            "/*!50003 CREATE*/ /*!50017 DEFINER=`root`@`localhost`*/ /*!50003 TRIGGER tr BEFORE INSERT ON t FOR EACH ROW SET NEW.a = 1 */",
            "/*!50001 DROP VIEW IF EXISTS `v`*/",
        ] {
            assert!(commits_implicitly(yes.as_bytes(), MYSQL), "{yes}");
        }
        for no in [
            "create temporary table t(a)",
            "DROP TEMPORARY TABLE t",
            "CREATE OR REPLACE TEMPORARY TABLE t (a int)",
            "LOAD DATA LOCAL INFILE 'x' INTO TABLE t",
            "insert into t values (1)",
            "select 1",
            "call p()",
            "set @a = 1",
            "update t set a = 1",
            "/*!40101 SET NAMES utf8mb4 */",
            "/*!99999 ALTER TABLE t DISABLE KEYS */",
            "/* ALTER TABLE t */ select 1",
            "START TRANSACTION",
        ] {
            assert!(!commits_implicitly(no.as_bytes(), MYSQL), "{no}");
        }
        // MariaDB reads `/*M!` as code, MySQL does not; MariaDB skips the MySQL-only versions of `/*!`.
        assert!(commits_implicitly(b"/*M!100000 ALTER TABLE t DISABLE KEYS */", MARIADB));
        assert!(!commits_implicitly(b"/*M!100000 ALTER TABLE t DISABLE KEYS */", MYSQL));
        assert!(!commits_implicitly(b"/*!80000 ALTER TABLE t DISABLE KEYS */", MARIADB));
        // An unknown server version reads a versioned comment as code.
        assert!(commits_implicitly(b"/*!99999 ALTER TABLE t DISABLE KEYS */", Comments { version: None, mariadb: false, ansi_quotes: false }));
    }

    fn repeats(text: &str, comments: Option<Comments>) -> bool {
        session_statement(text.as_bytes(), comments) == SessionStatement::Repeat
    }

    #[test]
    fn what_only_changes_the_session_is_repeated() {
        for yes in [
            "set session sql_mode = ''",
            "SET @a = 1",
            "SET @OLD_UNIQUE_CHECKS=@@UNIQUE_CHECKS, UNIQUE_CHECKS=0",
            "SET NAMES utf8mb4 COLLATE utf8mb4_general_ci",
            "/*!40101 SET @OLD_SQL_MODE=@@SQL_MODE, SQL_MODE='NO_AUTO_VALUE_ON_ZERO' */",
            "/*!50003 SET sql_mode = 'ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES' */",
            "SET SESSION TRANSACTION ISOLATION LEVEL READ COMMITTED",
            "SET FOREIGN_KEY_CHECKS=0",
            "SET @@SESSION.SQL_LOG_BIN= 0",
            "SET TIME_ZONE='+00:00'",
            "SET @x = DEFAULT",
            "SET @saved_cs_client = @@character_set_client",
            "USE `b`",
        ] {
            assert!(repeats(yes, Some(MYSQL)), "{yes}");
        }
        for yes in [
            "SET search_path = public",
            "SET search_path TO \"$user\", public",
            "SET statement_timeout = 0",
            "SET client_encoding = 'UTF8'",
            "SET row_security = off",
            "SET check_function_bodies = false",
            "SET TIME ZONE 'UTC'",
            "SELECT pg_catalog.set_config('search_path', '', false)",
            "select set_config('search_path', 'a, b', false);",
        ] {
            assert!(repeats(yes, None), "{yes}");
        }
    }

    #[test]
    fn what_changes_the_server_or_computes_its_value_is_not_repeated_and_says_why() {
        let reason = |text: &str, comments: Option<Comments>| match session_statement(text.as_bytes(), comments) {
            SessionStatement::NotRepeated(reason) => reason,
            other => panic!("{text}: {other:?}"),
        };
        for text in [
            "SET GLOBAL max_connections = 5",
            "SET PERSIST x = 1",
            "SET PERSIST_ONLY x = 1",
            "SET @@GLOBAL.max_connections = 5",
            "SET @@global.x=1",
            "SET @@PERSIST.x = 1",
            "SET @@GLOBAL.GTID_PURGED=/*!80000 '+'*/ '8f47:1-9';",
            "/*!80000 SET @@GLOBAL.GTID_PURGED='x' */",
            "SET SESSION x = 1, GLOBAL y = 2",
        ] {
            assert_eq!(reason(text, Some(MYSQL)), NOT_REPEATED_SERVER, "{text}");
        }
        for text in ["SET TRANSACTION READ ONLY", "SET LOCAL x = 1", "SET CONSTRAINTS ALL DEFERRED"] {
            assert_eq!(reason(text, None), NOT_REPEATED_TRANSACTION, "{text}");
        }
        for text in ["SET PASSWORD = 'x'", "SET ROLE admin", "SET DEFAULT ROLE ALL TO u"] {
            assert_eq!(reason(text, Some(MYSQL)), NOT_REPEATED_ACCOUNT, "{text}");
        }
        for text in ["SET autocommit = 0", "SET @@autocommit=1", "SET SESSION autocommit = OFF", "SET @@session.autocommit = 0"] {
            assert_eq!(reason(text, Some(MYSQL)), NOT_REPEATED_AUTOCOMMIT, "{text}");
        }
        for text in ["SET @id = LAST_INSERT_ID()", "SET @t = fn()", "SET @x = (SELECT 1)", "SET @a = @b * 2", "SET @s = CONCAT('a', 'b')", "SET x = /*!50000 5 */"] {
            assert_eq!(reason(text, Some(MYSQL)), NOT_REPEATED_COMPUTED, "{text}");
        }
        assert_eq!(reason("SET x = nextval('s')", None), NOT_REPEATED_COMPUTED);
    }

    #[test]
    fn statements_that_only_look_like_a_set_up_are_other() {
        for no in ["SELECT 1", "INSERT INTO t VALUES (1)", "SELECT reset_config('a', 'b')", "SELECT my_set_config('a', 'b', false)", "select set_config('a', (select 1), false)", "SELECT x FROM t WHERE set_config('a', 'b', false)", "RESET ALL", "SETTINGS x"] {
            assert_eq!(session_statement(no.as_bytes(), None), SessionStatement::Other, "{no}");
        }
        assert_eq!(session_statement(b"USE x", None), SessionStatement::Other, "PostgreSQL has no USE");
        let long = format!("SELECT {} FROM t WHERE set_config('a', 'b', false)", "x, ".repeat(500));
        assert_eq!(session_statement(long.as_bytes(), None), SessionStatement::Other, "a long SELECT is not scanned");
    }

    #[test]
    fn use_names_its_database_quoted_or_not_after_any_comment() {
        for (text, name) in [("USE b", "b"), ("use `my db`;", "my db"), ("/*!40101 USE `b``c` */", "b`c"), ("USE   x -- done", "x"), ("USE b;", "b"), ("USE b/*c*/", "b"), ("USE b--c", "b"), ("USE b#c", "b"), ("USE app", "app")] {
            assert_eq!(use_target(text.as_bytes(), MYSQL).as_deref(), Some(name), "{text}");
        }
        // A header of any length before it.
        let header = format!("/* {} */\n-- another\n", "x".repeat(2000));
        assert_eq!(use_target(format!("{header}USE app;").as_bytes(), MYSQL).as_deref(), Some("app"));
        // `"b"` is a name only under ANSI_QUOTES.
        assert_eq!(use_target(b"USE \"b\"", ANSI).as_deref(), Some("b"));
        assert_eq!(use_target(b"USE \"b\"", MYSQL), None);
        for text in ["USER b", "SELECT 1", "USE", "USE ``"] {
            assert_eq!(use_target(text.as_bytes(), MYSQL), None, "{text}");
        }
    }

    #[test]
    fn dropping_a_database_is_told_with_its_name() {
        for (text, name) in [("DROP DATABASE b", "b"), ("drop schema if exists `b`", "b"), ("/*!40000 DROP DATABASE IF EXISTS `my db` */", "my db"), ("DROP DATABASE IF EXISTS b -- x", "b")] {
            assert_eq!(drop_database_target(text.as_bytes(), MYSQL).as_deref(), Some(name), "{text}");
        }
        for text in ["DROP TABLE b", "CREATE DATABASE b", "DROP DATABASE"] {
            assert_eq!(drop_database_target(text.as_bytes(), MYSQL), None, "{text}");
        }
    }

    #[test]
    fn values_read_from_the_session_when_set_are_computed_and_a_call_must_be_the_whole_statement() {
        for computed in ["SET @id = @@IDENTITY", "SET @id = @@last_insert_id", "SET @t = CURRENT_TIMESTAMP", "SET @u = current_user", "SET @d = UTC_DATE"] {
            assert_eq!(session_statement(computed.as_bytes(), Some(MYSQL)), SessionStatement::NotRepeated(NOT_REPEATED_COMPUTED), "{computed}");
        }
        assert_eq!(session_statement(b"SELECT pg_catalog.set_config('search_path', '', false);", None), SessionStatement::Repeat);
        assert_eq!(session_statement(b"select set_config('a', 'b', false) ;\n", None), SessionStatement::Repeat);
        assert_eq!(session_statement(b"select set_config('a', 'b', false), my_side_effect()", None), SessionStatement::Other);
    }

    #[test]
    fn set_local_is_the_session_on_mysql_and_one_transaction_on_postgresql() {
        assert_eq!(session_statement(b"SET LOCAL sql_mode = ''", Some(MYSQL)), SessionStatement::Repeat);
        assert_eq!(session_statement(b"SET LOCAL x = 1", None), SessionStatement::NotRepeated(NOT_REPEATED_TRANSACTION));
        assert_eq!(session_statement(b"SET LOCAL autocommit = 0", Some(MYSQL)), SessionStatement::NotRepeated(NOT_REPEATED_AUTOCOMMIT));
        assert_eq!(transaction_of(b"SET SESSION\tautocommit = 0", Some(MYSQL)), Some(Transaction::AutocommitOff));
        assert_eq!(transaction_of(b"SET local  autocommit=1", Some(MYSQL)), Some(Transaction::AutocommitOn));
        assert_eq!(transaction_of(b"SET sessionx autocommit=1", Some(MYSQL)), None);
    }

    #[test]
    fn loading_an_index_and_importing_a_table_commit_and_loading_data_does_not() {
        for commits in ["LOAD INDEX INTO CACHE t", "IMPORT TABLE FROM '/tmp/t.sdi'"] {
            assert!(commits_implicitly(commits.as_bytes(), MYSQL), "{commits}");
        }
        assert!(!commits_implicitly(b"LOAD DATA LOCAL INFILE 'x' INTO TABLE t", MYSQL));
    }
}
