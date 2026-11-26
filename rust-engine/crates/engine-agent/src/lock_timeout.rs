//! `lock_timeout_secs` of `execute_query` and `execute_batch`: how long one call may wait for a lock
//! another session holds. The layer sets it around each statement of the call and takes it off
//! again on every path, so an agent can run an `ALTER` on a busy table without the
//! `SET lock_timeout` the policy refuses, and no later call inherits it. Everything happens inside
//! the statement's own busy window, so no other call can run in between.
//!
//! * **PostgreSQL, outside a transaction, a statement that can run in a transaction block:**
//!   `BEGIN`, `SET LOCAL lock_timeout = 'Ns'`, the statement, then `COMMIT` (`ROLLBACK` when it
//!   failed or was cancelled). `SET LOCAL` ends with the transaction, so nothing is left on the
//!   session even if a later step fails, and behind a pooler in transaction mode the four steps
//!   stay on one backend.
//! * **PostgreSQL, a statement that cannot run in a transaction block** (`CREATE INDEX
//!   CONCURRENTLY`, `VACUUM`, `CREATE DATABASE`, ...; see [`needs_autocommit`]): `SET lock_timeout`,
//!   the statement, `RESET lock_timeout`, as separate autocommit statements. Behind a
//!   transaction-mode pooler those can reach different backends: the schema text says so.
//! * **PostgreSQL, in a transaction** (`transaction_id`, `use_transaction`): `SET LOCAL`, then
//!   `SET LOCAL lock_timeout = DEFAULT`, so it does not outlast the call inside an open one.
//! * **PostgreSQL, a pinned session** (`session_id`, never in a transaction block): the same two
//!   paths as outside a transaction. The one that cannot use a transaction block puts back the
//!   `lock_timeout` the session had (the agent may have set its own with `SET`), read first with
//!   `current_setting`, instead of resetting it. A session that cannot be put back is closed (it is
//!   the pinned session that goes, and the error says so).
//! * **MySQL / MariaDB** (in a transaction or not, in a pinned session or not): the two settings
//!   are read in one `SELECT`, both are set, and both are put back as integer literals. The
//!   values are never kept in user variables, which a statement of the agent could overwrite. A
//!   statement that fails in a transaction aborts it for the agent (the layer only accepts a
//!   rollback then), so the settings are not put back after it: the session ends with the
//!   transaction.
//! * Every other engine rejects the parameter.
//!
//! Without the parameter none of this runs: no extra round trip.

use crate::cancel::{CancelToken, Watchdog};
use crate::error::AgentError;
use crate::limits::Limits;
use crate::ports::{ConnectionKind, Language, RunRequest, Session};
use engine_driver_kit::sql::{tokens, SqlLexOptions, SqlTokenKind};
use std::time::{Duration, Instant};

/// The most `lock_timeout_secs` accepts.
const MAX_SECS: u16 = 600;
/// How long taking the timeout off again waits for the server before the session is given up.
const RESTORE_TIMEOUT: Duration = Duration::from_secs(10);

const MYSQL_SAVE: &str =
    "SELECT @@SESSION.lock_wait_timeout, @@SESSION.innodb_lock_wait_timeout";

/// A validated `lock_timeout_secs` for a connection of a supported kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LockTimeout {
    kind: ConnectionKind,
    secs: u16,
}

/// Where and what one call runs.
pub(crate) struct Call<'a> {
    pub session: &'a dyn Session,
    /// The database the statement runs in (PostgreSQL keeps a session per database).
    pub database: Option<&'a str>,
    /// The statement runs in a transaction of the agent's (`transaction_id`, `use_transaction`).
    pub in_transaction: bool,
    /// The statement runs in a pinned session, whose own settings outlast the call.
    pub pinned: bool,
    pub statement: &'a str,
    pub lex: Option<SqlLexOptions>,
    /// The connection's statement timeout: what the call's watchdog gives it in all, and so what
    /// a `COMMIT` may still use.
    pub budget: Duration,
}

impl LockTimeout {
    /// `None` for no parameter. `invalid_arguments` outside 1 to 600 or not below the
    /// connection's statement timeout (which would cancel the call first); `unsupported` on an
    /// engine that has no such setting (it is never silently ignored).
    pub(crate) fn parse(
        kind: ConnectionKind,
        requested: Option<i64>,
        statement_timeout: Duration,
    ) -> Result<Option<Self>, AgentError> {
        let Some(secs) = requested else {
            return Ok(None);
        };
        let secs = u16::try_from(secs)
            .ok()
            .filter(|secs| (1..=MAX_SECS).contains(secs))
            .ok_or_else(|| {
                AgentError::invalid(format!(
                    "`lock_timeout_secs` must be a whole number of seconds from 1 to {MAX_SECS}, not {secs}"
                ))
            })?;
        match kind {
            ConnectionKind::PostgreSql | ConnectionKind::MySql | ConnectionKind::MariaDb => {}
            ConnectionKind::Sqlite
            | ConnectionKind::DuckDb
            | ConnectionKind::ClickHouse
            | ConnectionKind::MongoDb
            | ConnectionKind::Redis => {
                return Err(AgentError::Unsupported(format!(
                    "`lock_timeout_secs` is supported on PostgreSQL, MySQL and MariaDB connections only; this connection is {}. Send the call without it",
                    kind.driver_id()
                )))
            }
        }
        if u64::from(secs) >= statement_timeout.as_secs() {
            return Err(AgentError::invalid(format!(
                "`lock_timeout_secs` ({secs}) must be smaller than this connection's statement timeout ({} s), which would cancel the call before the lock wait ends. Use a smaller value",
                statement_timeout.as_secs()
            )));
        }
        Ok(Some(Self { kind, secs }))
    }

    fn postgres(self) -> bool {
        self.kind == ConnectionKind::PostgreSql
    }

    fn set_sql(self, in_transaction: bool) -> String {
        let secs = self.secs;
        match (self.postgres(), in_transaction) {
            (true, true) => format!("SET LOCAL lock_timeout = '{secs}s'"),
            (true, false) => format!("SET lock_timeout = '{secs}s'"),
            (false, _) => format!(
                "SET SESSION lock_wait_timeout = {secs}, SESSION innodb_lock_wait_timeout = {secs}"
            ),
        }
    }

    /// Runs `statement` with the timeout in force and takes it off again. The second value says
    /// the session could not be put back and must be closed (never in a transaction: there the
    /// error aborts it, and the layer says to roll it back). A server error at the `COMMIT` of the
    /// transaction tablex wrapped the statement in (a deferred constraint, a serialization failure)
    /// is an ordinary failure of the statement: that error, its own code, and the session stays.
    pub(crate) fn run<T>(
        self,
        call: &Call<'_>,
        cancel: &CancelToken,
        statement: impl Fn() -> Result<T, AgentError>,
    ) -> (Result<T, AgentError>, bool) {
        let started = Instant::now();
        let in_transaction = call.in_transaction;
        let wrapped = self.postgres()
            && !in_transaction
            && call.lex.is_some_and(|lex| !needs_autocommit(call.statement, &lex));
        let (mut outcome, mut cleanup) = self.attempt(call, cancel, wrapped, started, &statement);
        // A statement the lexer did not know cannot run in a transaction block: the wrapper was
        // rolled back, so nothing ran, and it goes once more on the session path.
        if wrapped
            && cleanup.is_ok()
            && matches!(&outcome, Err(AgentError::Driver { message, .. }) if refused_in_a_block(message))
        {
            (outcome, cleanup) = self.attempt(call, cancel, false, started, &statement);
        }
        match cleanup {
            Ok(()) => (outcome.map_err(|error| self.explain(error, in_transaction)), false),
            Err(Cleanup { refused: Some(error), .. }) if outcome.is_ok() => (
                Err(with_note(error, "The statement was rolled back with the transaction tablex ran it in")),
                false,
            ),
            Err(Cleanup { problem, .. }) => {
                let closed = if in_transaction {
                    String::new()
                } else if call.pinned {
                    ". The pinned session was closed and what it held is gone; open_session with pinned true opens a new one".to_owned()
                } else {
                    ". The session was closed; call open_session again".to_owned()
                };
                let error = match outcome {
                    Ok(_) if in_transaction => {
                        AgentError::driver("query.lock_timeout_not_lifted", problem, false)
                    }
                    Ok(_) => AgentError::NotConnected(format!("{problem}{closed}")),
                    Err(error) => with_note(self.explain(error, in_transaction), &format!("{problem}{closed}")),
                };
                (Err(error), !in_transaction)
            }
        }
    }

    /// One go: set the timeout, the statement, take the timeout off.
    fn attempt<T>(
        self,
        call: &Call<'_>,
        cancel: &CancelToken,
        wrapped: bool,
        started: Instant,
        statement: &impl Fn() -> Result<T, AgentError>,
    ) -> (Result<T, AgentError>, Result<(), Cleanup>) {
        let mut scope = Scope {
            session: call.session,
            database: call.database,
            in_transaction: call.in_transaction,
            undo: Undo::Nothing,
        };
        let outcome = match self.begin(&mut scope, call, cancel, wrapped) {
            Ok(()) => statement(),
            Err(error) => Err(error),
        };
        // A COMMIT may use what is left of the call's statement timeout (a deferred constraint
        // can take a while); anything else only puts the session back.
        let commit_budget = call.budget.saturating_sub(started.elapsed()).max(Duration::from_secs(1));
        let cleanup = scope.finish(outcome.is_ok(), commit_budget);
        (outcome, cleanup)
    }

    /// The statements before the one of the agent; records in `scope` what has to be undone.
    fn begin(
        self,
        scope: &mut Scope<'_>,
        call: &Call<'_>,
        cancel: &CancelToken,
        wrapped: bool,
    ) -> Result<(), AgentError> {
        let (session, database) = (call.session, call.database);
        if !self.postgres() {
            let (lock_wait, innodb) = saved_settings(session, database, cancel)?;
            scope.undo = Undo::Run(format!(
                "SET SESSION lock_wait_timeout = {lock_wait}, SESSION innodb_lock_wait_timeout = {innodb}"
            ));
        } else if call.in_transaction {
            scope.undo = Undo::Run("SET LOCAL lock_timeout = DEFAULT".into());
        } else if wrapped {
            // Armed first: a ROLLBACK is harmless when the BEGIN did not happen. One request, so a
            // pooler in transaction mode cannot split the BEGIN from its SET LOCAL.
            scope.undo = Undo::Transaction;
            return run(session, &format!("BEGIN; {}", self.set_sql(true)), database, cancel);
        } else {
            scope.undo = if call.pinned {
                // The session's own value goes back, not the default. Armed after the read: nothing
                // is set before it.
                Undo::Run(format!("SET lock_timeout = '{}'", saved_pg_timeout(session, database, cancel)?))
            } else {
                // A RESET is harmless when the SET did not happen (or was cancelled after it did).
                Undo::Run("RESET lock_timeout".into())
            };
        }
        run(session, &self.set_sql(call.in_transaction), database, cancel)
    }

    /// `error` as the agent should read it: a lock wait that ran out says so and says what to do.
    pub(crate) fn explain(self, error: AgentError, in_transaction: bool) -> AgentError {
        match error {
            AgentError::Driver { code, message, .. } if is_lock_timeout(&message) => {
                let next = if in_transaction {
                    "The transaction is aborted: roll it back, then retry the transaction later"
                } else {
                    "Retry later, or with a larger `lock_timeout_secs`"
                };
                AgentError::Driver {
                    code,
                    message: format!(
                        "{message}. The lock timeout was hit: another session holds a lock this statement needs, and it waited {} s (`lock_timeout_secs`). The statement did not complete. {next}",
                        self.secs
                    ),
                    retryable: !in_transaction,
                }
            }
            other => other,
        }
    }
}

/// `error` with `note` added to its message (the code stays).
fn with_note(error: AgentError, note: &str) -> AgentError {
    match error {
        AgentError::Driver { code, message, retryable } => AgentError::Driver {
            code,
            message: format!("{message}. {note}"),
            retryable,
        },
        other => other,
    }
}

/// The two settings, as they are now, for putting them back afterwards.
fn saved_settings(
    session: &dyn Session,
    database: Option<&str>,
    cancel: &CancelToken,
) -> Result<(u64, u64), AgentError> {
    let result = run_query(session, MYSQL_SAVE, database, cancel)?;
    let number = |at: usize| {
        result
            .rows
            .first()
            .and_then(|row| row.get(at))
            .and_then(|cell| cell.trim().parse::<u64>().ok())
    };
    match (number(0), number(1)) {
        (Some(lock_wait), Some(innodb)) => Ok((lock_wait, innodb)),
        _ => Err(AgentError::driver(
            "query.lock_timeout_unreadable",
            "the session's lock wait settings could not be read, so no lock timeout was set",
            false,
        )),
    }
}

/// The session's `lock_timeout` as PostgreSQL writes it (`0`, `5s`, `1min`), for putting it back as
/// it was. Only a plain number with a unit is accepted: it goes into a `SET`.
fn saved_pg_timeout(
    session: &dyn Session,
    database: Option<&str>,
    cancel: &CancelToken,
) -> Result<String, AgentError> {
    let result = run_query(session, "SELECT current_setting('lock_timeout')", database, cancel)?;
    result
        .rows
        .first()
        .and_then(|row| row.first())
        .map(|cell| cell.trim().to_owned())
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 32
                && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '.')
        })
        .ok_or_else(|| {
            AgentError::driver(
                "query.lock_timeout_unreadable",
                "the session's lock wait settings could not be read, so no lock timeout was set",
                false,
            )
        })
}

/// What PostgreSQL (`canceling statement due to lock timeout`, SQLSTATE 55P03; the driver's
/// error carries the text only) and MySQL / MariaDB (error 1205, `Lock wait timeout exceeded`;
/// the driver's text names the number) say when the wait ran out.
fn is_lock_timeout(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("(error 1205,")
        || message.contains("due to lock timeout")
        || message.contains("lock wait timeout exceeded")
}

/// PostgreSQL's refusal of a statement in a transaction block (SQLSTATE 25001; the driver's error
/// carries the text only).
fn refused_in_a_block(message: &str) -> bool {
    message.to_ascii_lowercase().contains("cannot run inside a transaction block")
}

/// Whether PostgreSQL cannot run `sql` inside a transaction block (so it is not wrapped in one):
/// anything `CONCURRENTLY` or `VACUUM`, `CALL` (a procedure may `COMMIT`), `CLUSTER`, `CREATE` /
/// `DROP` / `ALTER` of a `DATABASE`, `TABLESPACE`, `SYSTEM` or `SUBSCRIPTION`, and `REINDEX
/// DATABASE` / `SYSTEM` / `SCHEMA`. Leaning to "yes" is safe: that path works for every statement,
/// and one this misses is retried there when PostgreSQL refuses it in a block ([`run`](LockTimeout::run)).
fn needs_autocommit(sql: &str, lex: &SqlLexOptions) -> bool {
    let scan = tokens(sql, lex);
    let words: Vec<String> = scan
        .tokens
        .iter()
        .filter(|token| token.kind == SqlTokenKind::Word)
        .map(|token| sql[token.range.clone()].to_ascii_uppercase())
        .collect();
    if scan.unterminated || words.iter().any(|word| word == "CONCURRENTLY" || word == "VACUUM") {
        return true;
    }
    let word = |at: usize| words.get(at).map(String::as_str);
    matches!(
        (word(0), word(1)),
        (Some("CALL" | "CLUSTER"), _)
            | (
                Some("CREATE" | "DROP" | "ALTER"),
                Some("DATABASE" | "TABLESPACE" | "SYSTEM" | "SUBSCRIPTION")
            )
            | (Some("REINDEX"), Some("DATABASE" | "SYSTEM" | "SCHEMA"))
    )
}

/// One internal statement, outside the agent policy (like `Session::control`).
fn run_query(
    session: &dyn Session,
    sql: &str,
    database: Option<&str>,
    cancel: &CancelToken,
) -> Result<crate::result::QueryResult, AgentError> {
    session.run(
        &RunRequest {
            language: Language::Sql,
            text: sql,
            database,
            limits: &Limits::default(),
            row_limit: 1,
            confirmed: true,
            read_only: false,
        },
        cancel,
    )
}

fn run(
    session: &dyn Session,
    sql: &str,
    database: Option<&str>,
    cancel: &CancelToken,
) -> Result<(), AgentError> {
    run_query(session, sql, database, cancel).map(drop)
}

/// What puts the session back.
enum Undo {
    Nothing,
    /// This statement.
    Run(String),
    /// The transaction tablex began around the statement: `COMMIT` after a success, `ROLLBACK`
    /// after anything else.
    Transaction,
}

/// Taking the timeout off failed.
struct Cleanup {
    /// What is left undone, in words for the agent.
    problem: String,
    /// The `COMMIT` of the wrapping transaction was answered with a server error: the transaction
    /// is over (rolled back, and its `SET LOCAL` with it) and the session is fine.
    refused: Option<AgentError>,
}

/// A lock timeout in force on a session. [`finish`](Self::finish) takes it off and says if that
/// failed; `Drop` is only the fallback for a panic, where nothing calls the driver: the layer
/// closes the session (`CloseOnUnwind`).
struct Scope<'a> {
    session: &'a dyn Session,
    database: Option<&'a str>,
    in_transaction: bool,
    undo: Undo,
}

impl Scope<'_> {
    /// Puts the session back, with a token of its own (the call's may be cancelled already). In a
    /// transaction after a failure there is nothing to do: it is aborted and only a rollback is
    /// left.
    fn finish(&mut self, statement_ok: bool, commit_budget: Duration) -> Result<(), Cleanup> {
        let (sql, budget) = match std::mem::replace(&mut self.undo, Undo::Nothing) {
            Undo::Nothing => return Ok(()),
            Undo::Run(_) if self.in_transaction && !statement_ok => return Ok(()),
            Undo::Run(sql) => (sql, RESTORE_TIMEOUT),
            Undo::Transaction if statement_ok => ("COMMIT".to_owned(), commit_budget),
            Undo::Transaction => ("ROLLBACK".to_owned(), RESTORE_TIMEOUT),
        };
        let token = CancelToken::new();
        let _watchdog = Watchdog::arm(&token, budget);
        run(self.session, &sql, self.database, &token).map_err(|error| match sql.as_str() {
            // The server answered: the transaction is over, whatever it said.
            "COMMIT" if matches!(error, AgentError::Driver { .. }) => Cleanup {
                problem: String::new(),
                refused: Some(error),
            },
            // No answer, or a cancel: nobody can say whether it committed.
            "COMMIT" => Cleanup {
                problem: format!("the statement ran inside a transaction that tablex could not commit ({error}); its effect is not certain, so check it with a read"),
                refused: None,
            },
            "ROLLBACK" => Cleanup {
                problem: format!("the transaction around the statement could not be rolled back ({error})"),
                refused: None,
            },
            _ => Cleanup {
                problem: format!("the statement ran, but its lock timeout could not be taken off the session ({error})"),
                refused: None,
            },
        })
    }
}

impl Drop for Scope<'_> {
    fn drop(&mut self) {
        if matches!(self.undo, Undo::Nothing) || std::thread::panicking() {
            return;
        }
        // An early return that skipped `finish`: not reachable today; put the session back or drop it.
        if self.finish(false, RESTORE_TIMEOUT).is_err() && !self.in_transaction {
            self.session.disconnect();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{Behavior, FakeSession};
    use ConnectionKind::*;

    const LONG: Duration = Duration::from_secs(30);

    fn timeout(kind: ConnectionKind, secs: i64) -> LockTimeout {
        LockTimeout::parse(kind, Some(secs), LONG).unwrap().unwrap()
    }

    fn runs(session: &FakeSession) -> Vec<String> {
        session.runs.lock().unwrap().iter().map(|(text, _)| text.clone()).collect()
    }

    fn lex() -> Option<SqlLexOptions> {
        Some(SqlLexOptions::postgres())
    }

    /// A fake whose statements all succeed, and whose settings read as `(31536000, 50)`.
    fn healthy(kind: ConnectionKind) -> FakeSession {
        let session = FakeSession::new(kind);
        session.set_behavior(Behavior::Script(Box::new(|request, _| {
            let mut collector = crate::limits::RowCollector::new(
                Vec::new(),
                request.row_limit,
                request.limits,
            );
            if request.text.starts_with("SELECT @@SESSION") {
                let _ = collector.push_row(["31536000".to_owned(), "50".to_owned()]);
            }
            Ok(collector.finish(false, Some(0)))
        })));
        session
    }

    #[test]
    fn the_parameter_is_checked_before_anything_runs() {
        assert_eq!(LockTimeout::parse(PostgreSql, None, LONG), Ok(None));
        assert_eq!(LockTimeout::parse(Sqlite, None, LONG), Ok(None));
        for secs in [1, 5, 29] {
            assert!(LockTimeout::parse(PostgreSql, Some(secs), LONG).unwrap().is_some());
        }
        assert!(LockTimeout::parse(PostgreSql, Some(600), Duration::from_secs(900)).unwrap().is_some());
        for secs in [0, -1, 601, 65_541, i64::MAX] {
            let error = LockTimeout::parse(PostgreSql, Some(secs), LONG).unwrap_err();
            assert_eq!(error.code(), "agent.invalid_arguments", "{secs}");
            assert!(error.to_string().contains("from 1 to 600"), "{secs}");
        }
        for kind in [MySql, MariaDb] {
            assert!(LockTimeout::parse(kind, Some(5), LONG).unwrap().is_some());
        }
        for kind in [Sqlite, ClickHouse, MongoDb, Redis] {
            let error = LockTimeout::parse(kind, Some(5), LONG).unwrap_err();
            assert_eq!(error.code(), "agent.unsupported");
            let text = error.to_string();
            assert!(text.contains("PostgreSQL, MySQL and MariaDB"), "{text}");
            assert!(text.contains(kind.driver_id()), "{text}");
        }
        // The range is judged before the kind.
        assert_eq!(
            LockTimeout::parse(Sqlite, Some(0), LONG).unwrap_err().code(),
            "agent.invalid_arguments"
        );
    }

    #[test]
    fn a_timeout_the_statement_timeout_would_cut_off_is_refused() {
        for secs in [30, 31, 600] {
            let error = LockTimeout::parse(PostgreSql, Some(secs), LONG).unwrap_err();
            assert_eq!(error.code(), "agent.invalid_arguments", "{secs}");
            let text = error.to_string();
            assert!(text.contains("statement timeout (30 s)") && text.contains("smaller value"), "{text}");
        }
        assert!(LockTimeout::parse(MySql, Some(2), Duration::from_secs(2)).is_err());
        assert!(LockTimeout::parse(MySql, Some(1), Duration::from_secs(2)).is_ok());
    }

    #[test]
    fn what_cannot_run_in_a_transaction_block_is_found_by_the_lexer() {
        let lex = SqlLexOptions::postgres();
        for sql in [
            "CREATE INDEX CONCURRENTLY i ON t (a)",
            "drop index concurrently i",
            "REINDEX TABLE CONCURRENTLY t",
            "ALTER TABLE p DETACH PARTITION q CONCURRENTLY",
            "VACUUM t",
            "VACUUM (ANALYZE) t",
            "CREATE DATABASE d",
            "DROP DATABASE d",
            "ALTER SYSTEM SET work_mem = '1MB'",
            "CREATE TABLESPACE s LOCATION '/x'",
            "ALTER DATABASE d SET TABLESPACE s",
            "REINDEX DATABASE d",
            "CALL archive()",
            "CLUSTER t USING i",
            "CLUSTER",
            "REINDEX SCHEMA s",
            "/* c */ CREATE INDEX CONCURRENTLY i ON t (a)",
            "SELECT 'unterminated",
        ] {
            assert!(needs_autocommit(sql, &lex), "{sql}");
        }
        for sql in [
            "ALTER TABLE t ADD COLUMN c text",
            "ALTER TABLE t ADD COLUMN system text",
            "CREATE TABLE database (a int)",
            "CREATE INDEX i ON t (a)",
            "SELECT 'CONCURRENTLY, VACUUM' FROM t -- VACUUM",
            "INSERT INTO t VALUES (1)",
            "REINDEX TABLE t",
            "UPDATE t SET a = 1 WHERE id = 1",
        ] {
            assert!(!needs_autocommit(sql, &lex), "{sql}");
        }
    }

    #[test]
    fn postgres_wraps_a_statement_in_one_transaction() {
        let session = healthy(PostgreSql);
        let call = Call {
            session: &session,
            database: Some("app"),
            in_transaction: false,
            statement: "ALTER TABLE t ADD COLUMN c text",
            lex: lex(),
            pinned: false,
            budget: LONG,
        };
        let (result, close) = timeout(PostgreSql, 5).run(&call, &CancelToken::new(), || Ok(7));
        assert_eq!(result, Ok(7));
        assert!(!close);
        assert_eq!(runs(&session), ["BEGIN; SET LOCAL lock_timeout = '5s'", "COMMIT"]);

        // A failure rolls it back, and the statement's error is the answer.
        let session = healthy(PostgreSql);
        let call = Call { session: &session, ..call };
        let (result, close) = timeout(PostgreSql, 5)
            .run(&call, &CancelToken::new(), || Err::<(), _>(AgentError::Cancelled));
        assert_eq!(result, Err(AgentError::Cancelled));
        assert!(!close);
        assert_eq!(runs(&session), ["BEGIN; SET LOCAL lock_timeout = '5s'", "ROLLBACK"]);
    }

    #[test]
    fn a_statement_that_cannot_be_wrapped_uses_set_and_reset() {
        let session = healthy(PostgreSql);
        let call = Call {
            session: &session,
            database: None,
            in_transaction: false,
            statement: "CREATE INDEX CONCURRENTLY i ON t (a)",
            lex: lex(),
            pinned: false,
            budget: LONG,
        };
        let (result, _) = timeout(PostgreSql, 5).run(&call, &CancelToken::new(), || Ok(()));
        assert!(result.is_ok());
        assert_eq!(runs(&session), ["SET lock_timeout = '5s'", "RESET lock_timeout"]);
    }

    /// A fake whose `lock_timeout` reads as `value`.
    fn with_pg_timeout(value: &'static str) -> FakeSession {
        let session = FakeSession::new(PostgreSql);
        session.set_behavior(Behavior::Script(Box::new(move |request, _| {
            let mut collector =
                crate::limits::RowCollector::new(Vec::new(), request.row_limit, request.limits);
            if request.text.starts_with("SELECT current_setting") {
                let _ = collector.push_row([value.to_owned()]);
            }
            Ok(collector.finish(false, Some(0)))
        })));
        session
    }

    #[test]
    fn a_pinned_session_gets_its_own_lock_timeout_back() {
        // A statement that can run in a block is wrapped: SET LOCAL leaves the session's value alone.
        let session = healthy(PostgreSql);
        let call = Call {
            session: &session,
            database: None,
            in_transaction: false,
            pinned: true,
            statement: "ALTER TABLE t ADD COLUMN c text",
            lex: lex(),
            budget: LONG,
        };
        let (result, close) = timeout(PostgreSql, 5).run(&call, &CancelToken::new(), || Ok(()));
        assert!(result.is_ok() && !close);
        assert_eq!(runs(&session), ["BEGIN; SET LOCAL lock_timeout = '5s'", "COMMIT"]);
        // One that cannot: the value the pinned session had (here the agent's own `SET`) goes back,
        // not the default.
        let session = with_pg_timeout("2s");
        let call = Call {
            session: &session,
            statement: "CREATE INDEX CONCURRENTLY i ON t (a)",
            ..call
        };
        let (result, close) = timeout(PostgreSql, 5).run(&call, &CancelToken::new(), || Ok(()));
        assert!(result.is_ok() && !close);
        assert_eq!(
            runs(&session),
            [
                "SELECT current_setting('lock_timeout')",
                "SET lock_timeout = '5s'",
                "SET lock_timeout = '2s'",
            ]
        );
        // A value that is not a plain number with a unit is never sent back: nothing is set.
        let session = with_pg_timeout("1s'; DROP TABLE t; --");
        let call = Call { session: &session, ..call };
        let ran = std::cell::Cell::new(false);
        let (result, close) = timeout(PostgreSql, 5).run(&call, &CancelToken::new(), || {
            ran.set(true);
            Ok(())
        });
        assert_eq!(result.unwrap_err().code(), "query.lock_timeout_unreadable");
        assert!(!close && !ran.get());
        assert_eq!(runs(&session), ["SELECT current_setting('lock_timeout')"]);
    }

    #[test]
    fn a_pinned_session_that_cannot_be_put_back_is_closed_and_the_error_says_so() {
        let session = FakeSession::new(PostgreSql);
        session.set_behavior(Behavior::Script(Box::new(|request, _| {
            let mut collector =
                crate::limits::RowCollector::new(Vec::new(), request.row_limit, request.limits);
            if request.text.starts_with("SELECT current_setting") {
                let _ = collector.push_row(["0".to_owned()]);
            }
            if request.text == "SET lock_timeout = '0'" {
                return Err(AgentError::NotConnected("the connection was lost".into()));
            }
            Ok(collector.finish(false, Some(0)))
        })));
        let call = Call {
            session: &session,
            database: None,
            in_transaction: false,
            pinned: true,
            statement: "VACUUM t",
            lex: lex(),
            budget: LONG,
        };
        let (result, close) = timeout(PostgreSql, 5).run(&call, &CancelToken::new(), || Ok(()));
        assert!(close);
        let text = result.unwrap_err().to_string();
        assert!(text.contains("pinned session was closed") && text.contains("pinned true"), "{text}");
    }

    #[test]
    fn in_a_transaction_the_timeout_is_local_and_lifted_after_a_success() {
        let session = healthy(PostgreSql);
        let call = Call {
            session: &session,
            database: None,
            in_transaction: true,
            statement: "ALTER TABLE t ADD COLUMN c text",
            lex: lex(),
            pinned: false,
            budget: LONG,
        };
        let (result, _) = timeout(PostgreSql, 5).run(&call, &CancelToken::new(), || Ok(()));
        assert!(result.is_ok());
        assert_eq!(
            runs(&session),
            ["SET LOCAL lock_timeout = '5s'", "SET LOCAL lock_timeout = DEFAULT"]
        );
        // After a failure the transaction is aborted: nothing more is sent.
        let session = healthy(PostgreSql);
        let call = Call { session: &session, ..call };
        let (result, close) = timeout(PostgreSql, 5)
            .run(&call, &CancelToken::new(), || Err::<(), _>(AgentError::Cancelled));
        assert!(result.is_err() && !close);
        assert_eq!(runs(&session), ["SET LOCAL lock_timeout = '5s'"]);
    }

    #[test]
    fn mysql_reads_both_settings_and_puts_them_back_as_numbers() {
        for kind in [MySql, MariaDb] {
            let session = healthy(kind);
            let call = Call {
                session: &session,
                database: None,
                in_transaction: false,
                statement: "ALTER TABLE t ADD COLUMN c text",
                lex: kind.lex_options(),
                pinned: false,
            budget: LONG,
            };
            let (result, close) = timeout(kind, 7).run(&call, &CancelToken::new(), || Ok(()));
            assert!(result.is_ok() && !close);
            assert_eq!(
                runs(&session),
                [
                    MYSQL_SAVE,
                    "SET SESSION lock_wait_timeout = 7, SESSION innodb_lock_wait_timeout = 7",
                    "SET SESSION lock_wait_timeout = 31536000, SESSION innodb_lock_wait_timeout = 50",
                ]
            );
            assert!(runs(&session).iter().all(|text| !text.contains('@') || text == MYSQL_SAVE));
        }
        // Settings that cannot be read: nothing is set.
        let session = FakeSession::new(MariaDb);
        let call = Call {
            session: &session,
            database: None,
            in_transaction: false,
            statement: "SELECT 1",
            lex: MariaDb.lex_options(),
            pinned: false,
            budget: LONG,
        };
        let (result, close) = timeout(MariaDb, 7).run(&call, &CancelToken::new(), || Ok(()));
        assert_eq!(result.unwrap_err().code(), "query.lock_timeout_unreadable");
        assert!(!close);
        assert_eq!(runs(&session), [MYSQL_SAVE]);
    }

    #[test]
    fn a_lock_timeout_says_so_and_what_to_do() {
        let pg = timeout(PostgreSql, 5);
        let hit = || {
            AgentError::driver("query.failed", "ERROR: canceling statement due to lock timeout", false)
        };
        let explained = pg.explain(hit(), false);
        let text = explained.to_string();
        assert!(text.contains("The lock timeout was hit"), "{text}");
        assert!(text.contains("waited 5 s") && text.contains("Retry later"), "{text}");
        assert!(explained.retryable());
        assert_eq!(explained.code(), "query.failed");
        // In a transaction: roll it back first.
        let text = pg.explain(hit(), true).to_string();
        assert!(text.contains("roll it back, then retry the transaction"), "{text}");
        assert!(!text.contains("Retry later"), "{text}");
        assert!(!pg.explain(hit(), true).retryable());
        // MySQL / MariaDB: by the error number the driver names, and by the text.
        for message in [
            "Lock wait timeout exceeded; try restarting transaction (error 1205, SQLSTATE HY000)",
            "something (error 1205, SQLSTATE HY000)",
        ] {
            let my = timeout(MariaDb, 3).explain(AgentError::driver("query.failed", message, false), false);
            assert!(my.to_string().contains("Retry later") && my.retryable(), "{message}");
        }
        // Any other error is left as it is.
        let other = AgentError::driver("query.failed", "syntax error (error 1064, SQLSTATE 42000)", false);
        assert_eq!(pg.explain(other.clone(), false), other);
        assert_eq!(pg.explain(AgentError::Cancelled, false), AgentError::Cancelled);
    }

    #[test]
    fn a_failed_commit_after_a_success_is_an_error_and_closes_the_session() {
        let session = FakeSession::new(PostgreSql);
        session.set_behavior(Behavior::Script(Box::new(|request, _| {
            if request.text == "COMMIT" {
                Err(AgentError::NotConnected("the connection was lost".into()))
            } else {
                Ok(crate::limits::RowCollector::new(Vec::new(), 0, request.limits).finish(false, Some(0)))
            }
        })));
        let call = Call {
            session: &session,
            database: None,
            in_transaction: false,
            statement: "UPDATE t SET a = 1 WHERE id = 1",
            lex: lex(),
            pinned: false,
            budget: LONG,
        };
        let (result, close) = timeout(PostgreSql, 5).run(&call, &CancelToken::new(), || Ok(()));
        let error = result.unwrap_err();
        assert!(close);
        assert_eq!(error.code(), "agent.not_connected");
        let text = error.to_string();
        assert!(text.contains("could not commit") && text.contains("session was closed"), "{text}");
    }

    #[test]
    fn a_failed_lift_inside_a_transaction_is_an_error_without_closing() {
        let session = FakeSession::new(PostgreSql);
        session.set_behavior(Behavior::Script(Box::new(|request, _| {
            if request.text.ends_with("= DEFAULT") {
                Err(AgentError::driver("query.failed", "boom", false))
            } else {
                Ok(crate::limits::RowCollector::new(Vec::new(), 0, request.limits).finish(false, Some(0)))
            }
        })));
        let call = Call {
            session: &session,
            database: None,
            in_transaction: true,
            statement: "UPDATE t SET a = 1 WHERE id = 1",
            lex: lex(),
            pinned: false,
            budget: LONG,
        };
        let (result, close) = timeout(PostgreSql, 5).run(&call, &CancelToken::new(), || Ok(()));
        assert_eq!(result.unwrap_err().code(), "query.lock_timeout_not_lifted");
        assert!(!close);
        assert_eq!(session.disconnects.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn a_failed_set_is_the_error_and_the_transaction_is_rolled_back() {
        let session = FakeSession::new(PostgreSql);
        session.set_behavior(Behavior::Script(Box::new(|request, _| {
            if request.text.contains("SET LOCAL") {
                Err(AgentError::Cancelled)
            } else {
                Ok(crate::limits::RowCollector::new(Vec::new(), 0, request.limits).finish(false, Some(0)))
            }
        })));
        let call = Call {
            session: &session,
            database: None,
            in_transaction: false,
            statement: "SELECT 1",
            lex: lex(),
            pinned: false,
            budget: LONG,
        };
        let ran = std::cell::Cell::new(false);
        let (result, _) = timeout(PostgreSql, 5).run(&call, &CancelToken::new(), || {
            ran.set(true);
            Ok(())
        });
        assert_eq!(result, Err(AgentError::Cancelled));
        assert!(!ran.get(), "the statement does not run without its timeout");
        assert_eq!(runs(&session), ["BEGIN; SET LOCAL lock_timeout = '5s'", "ROLLBACK"]);
    }

    #[test]
    fn a_panic_sends_nothing_to_the_driver() {
        let session = healthy(PostgreSql);
        let call = Call {
            session: &session,
            database: None,
            in_transaction: false,
            statement: "SELECT 1",
            lex: lex(),
            pinned: false,
            budget: LONG,
        };
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = timeout(PostgreSql, 5).run(&call, &CancelToken::new(), || -> Result<(), AgentError> {
                panic!("driver bug")
            });
        }));
        assert!(caught.is_err());
        // Nothing is sent while panicking; the layer closes the session (`CloseOnUnwind`).
        assert_eq!(runs(&session), ["BEGIN; SET LOCAL lock_timeout = '5s'"]);
        assert_eq!(session.disconnects.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn a_server_error_at_commit_is_the_statements_error_and_the_session_stays() {
        let session = FakeSession::new(PostgreSql);
        session.set_behavior(Behavior::Script(Box::new(|request, _| {
            if request.text == "COMMIT" {
                Err(AgentError::driver(
                    "query.failed",
                    "ERROR: insert or update on table \"c\" violates foreign key constraint \"c_p\"",
                    true,
                ))
            } else {
                Ok(crate::limits::RowCollector::new(Vec::new(), 0, request.limits).finish(false, Some(1)))
            }
        })));
        let call = Call {
            session: &session,
            database: None,
            in_transaction: false,
            statement: "INSERT INTO c VALUES (1)",
            lex: lex(),
            pinned: false,
            budget: LONG,
        };
        let (result, close) = timeout(PostgreSql, 5).run(&call, &CancelToken::new(), || Ok(()));
        let error = result.unwrap_err();
        assert!(!close, "the transaction is over; the session is fine");
        assert_eq!(error.code(), "query.failed");
        assert!(error.retryable(), "its own flag");
        let text = error.to_string();
        assert!(text.contains("violates foreign key") && text.contains("rolled back"), "{text}");
        assert_eq!(session.disconnects.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn a_statement_postgres_refuses_in_a_block_is_retried_once_on_the_session_path() {
        let session = FakeSession::new(PostgreSql);
        let refused = std::sync::atomic::AtomicBool::new(false);
        session.set_behavior(Behavior::Script(Box::new(move |request, _| {
            if request.text.starts_with("REINDEX") && !refused.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return Err(AgentError::driver(
                    "query.failed",
                    "ERROR: REINDEX SCHEMA cannot run inside a transaction block",
                    false,
                ));
            }
            Ok(crate::limits::RowCollector::new(Vec::new(), 0, request.limits).finish(false, Some(0)))
        })));
        let statement = "REINDEX (VERBOSE) SCHEMA s";
        let call = Call {
            session: &session,
            database: None,
            in_transaction: false,
            statement,
            lex: lex(),
            pinned: false,
            budget: LONG,
        };
        let tries = std::cell::Cell::new(0);
        let (result, close) = timeout(PostgreSql, 5).run(&call, &CancelToken::new(), || {
            tries.set(tries.get() + 1);
            session
                .run(
                    &crate::ports::RunRequest {
                        language: Language::Sql,
                        text: statement,
                        database: None,
                        limits: &Limits::default(),
                        row_limit: 1,
                        confirmed: true,
                        read_only: false,
                    },
                    &CancelToken::new(),
                )
                .map(drop)
        });
        assert!(result.is_ok() && !close, "{result:?}");
        assert_eq!(tries.get(), 2);
        assert_eq!(
            runs(&session),
            [
                "BEGIN; SET LOCAL lock_timeout = '5s'",
                statement,
                "ROLLBACK",
                "SET lock_timeout = '5s'",
                statement,
                "RESET lock_timeout",
            ]
        );
        // Any other failure is not retried.
        let session = healthy(PostgreSql);
        let call = Call { session: &session, ..call };
        let tries = std::cell::Cell::new(0);
        let (result, _) = timeout(PostgreSql, 5).run(&call, &CancelToken::new(), || {
            tries.set(tries.get() + 1);
            Err::<(), _>(AgentError::driver("query.failed", "syntax error", false))
        });
        assert!(result.is_err());
        assert_eq!(tries.get(), 1);
    }
}
