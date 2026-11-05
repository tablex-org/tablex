//! The run's session: a second session of the connection (never a tab's own, never the shared one) that sends
//! the statements of the file one by one through the driver's SQL query, discards what they return, and answers
//! the psql lines it can (`\connect`).
//!
//! Every statement is sent with `run_to_end` (capability `query.run_to_end`): the driver runs it exactly as written,
//! never wrapped in a paging subquery or given a `LIMIT`, and reads every row it produces (keeping a page of them), so
//! `SELECT my_function(id) FROM t` runs for every row. A driver without the capability is refused up front.
//!
//! The request is written straight from the statement's text (one copy, no JSON value tree): a statement can be
//! megabytes long, and a file of millions of them should not build a map for each. Memory near the 256 MiB cap on a
//! statement: the reader holds the statement (up to the cap, plus the growth of its buffer), the request is a second copy
//! of it, the C string the driver is given a third, and the driver parses it once more: about four times the statement
//! at the peak, so a statement at the cap runs with roughly 1 GiB of the process's memory.

use std::sync::Arc;

use engine_driver_api::sql::codes;
use engine_driver_api::{CancelRequest, Capability, OperationId, QueryPage, SqlDialectInfo};
use engine_driver_host::{DriverHandle, DriverSession, LoaderError};
use serde::Serialize;

use super::head::{drop_database_target, use_target, Comments};
use super::run::{CommandResult, Done, Executor, Failure};
use crate::app::data_import::{first_cell, message_of};
use crate::app::structure_edit::apply::{ApplyControl, SessionSpec};
use crate::connections::ConnectionKind;

#[derive(Serialize)]
struct Request<'a> {
    kind: &'static str,
    operation_id: OperationId,
    database: Option<&'a str>,
    object: Option<&'a str>,
    offset: u64,
    limit: u64,
    payload: Payload<'a>,
}

#[derive(Serialize)]
struct Payload<'a> {
    sql: &'a str,
    /// Capability `query.run_to_end`.
    run_to_end: bool,
    /// The user chose to run the file: a statement the driver would ask about (`DROP`, `DELETE` without `WHERE`)
    /// is part of it.
    confirmed: bool,
}

/// Why the run's session could not be made ready.
#[derive(Debug)]
pub(in crate::app) enum OpenError {
    /// The connection could not be opened.
    Session(String),
    /// The driver cannot run a statement as written (it does not advertise `query.run_to_end`): it is older than this feature.
    DriverTooOld,
    /// The schema could not be put on the search path.
    Schema { schema: String, failure: Failure },
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Session(message) => f.write_str(message),
            Self::DriverTooOld => f.write_str("This connection's driver cannot run a statement exactly as written (it has no query.run_to_end): update it in the Driver Manager."),
            Self::Schema { schema, failure } => write!(f, "Could not select the schema {schema}: {failure}"),
        }
    }
}

impl std::error::Error for OpenError {}

pub(in crate::app) struct DriverExecutor {
    session: DriverSession,
    handle: Arc<DriverHandle>,
    control: Arc<ApplyControl>,
    kind: ConnectionKind,
    /// The database the next statement is sent for: the one chosen, then the one a `USE` or `\connect` of the file selects.
    database: Option<String>,
    /// MySQL and MariaDB: how a `USE` hidden in a versioned comment is read.
    mysql: Option<Comments>,
    /// This session's own SSH tunnel, when the connection has one. Declared last: it goes after the session
    /// has disconnected through it.
    _tunnel: Option<engine_connect::connect::LiveTunnel>,
}

impl DriverExecutor {
    /// Blocking: opens the session on `database`, and for PostgreSQL puts `schema` first on the search path (the rest of the
    /// path stays: `public` and `"$user"` are still found).
    pub(in crate::app) fn open(spec: SessionSpec, kind: ConnectionKind, database: Option<String>, schema: Option<&str>, control: Arc<ApplyControl>) -> Result<Self, OpenError> {
        let handle = Arc::clone(&spec.handle);
        if !handle.capabilities().has(Capability::QueryRunToEnd) {
            return Err(OpenError::DriverTooOld);
        }
        let (session, tunnel) = spec.open().map_err(OpenError::Session)?;
        let mysql = matches!(kind, ConnectionKind::Mysql | ConnectionKind::Mariadb).then(|| Comments {
            version: session.server_version(),
            mariadb: kind == ConnectionKind::Mariadb,
            ansi_quotes: session.connection_dialect().is_some_and(|dialect| dialect.double_quote_identifiers),
        });
        let mut executor = Self { session, handle, control, kind, database: database.filter(|database| !database.is_empty()), mysql, _tunnel: tunnel };
        if let (ConnectionKind::Postgres, Some(schema)) = (kind, schema.filter(|schema| !schema.is_empty())) {
            executor.put_schema_first(schema).map_err(|failure| OpenError::Schema { schema: schema.to_owned(), failure })?;
        }
        Ok(executor)
    }

    /// `schema` ahead of what the search path already holds, unless it is first.
    fn put_schema_first(&mut self, schema: &str) -> Result<(), Failure> {
        let current = first_cell(&mut self.session, self.database.clone(), "SELECT current_setting('search_path')");
        if current.as_deref().and_then(first_schema).is_some_and(|first| first == schema) {
            return Ok(());
        }
        self.send(&search_path_sql(schema), false).map(|_| ())
    }

    /// How MySQL and MariaDB read a versioned comment on this server; for the run's own reading of the file.
    pub(in crate::app) fn comments(&self) -> Option<Comments> {
        self.mysql
    }

    pub(in crate::app) fn dialect(&self) -> Option<&SqlDialectInfo> {
        self.session.connection_dialect()
    }

    pub(in crate::app) fn server_version(&self) -> Option<u32> {
        self.session.server_version()
    }

    fn failure(&self, error: &LoaderError) -> Failure {
        let text = crate::sessions::redact_secrets(&error.to_string());
        match error {
            LoaderError::DriverRejected { code, .. } if code.ends_with(".cancelled") => Failure::Cancelled,
            _ if self.control.is_cancelled() => Failure::Cancelled,
            LoaderError::DriverRejected { code, .. } if crate::sessions::is_connection_lost_code(code) || code == codes::MUTATION_COMMIT_UNKNOWN => Failure::Lost(text),
            // The server (or the driver, for text it cannot take) refused this statement; the session is fine.
            LoaderError::DriverRejected { .. } => Failure::Rejected(message_of(&text)),
            _ => Failure::Lost(text),
        }
    }

    /// Sends `sql` as an operation the Cancel button can reach (`cancellable`; not COMMIT and ROLLBACK).
    fn send(&mut self, sql: &str, cancellable: bool) -> Result<QueryPage, Failure> {
        if cancellable && self.control.is_cancelled() {
            return Err(Failure::Cancelled);
        }
        let operation = self.session.begin_operation().map_err(|error| self.failure(&error))?;
        if cancellable {
            let (handle, connection_id, operation_id) = (Arc::clone(&self.handle), self.session.connection_id(), operation.id());
            let canceller: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
                let _ = handle.cancel_operation(connection_id, &CancelRequest { operation_id });
            });
            self.control.register(canceller).map_err(|_| Failure::Cancelled)?;
        }
        let request = Request {
            kind: "sql",
            operation_id: operation.id(),
            database: self.database.as_deref(),
            object: None,
            offset: 0,
            limit: 1,
            payload: Payload { sql, run_to_end: true, confirmed: true },
        };
        let json = serde_json::to_string(&request).map_err(|error| Failure::Lost(error.to_string()));
        let page = json.and_then(|json| self.session.open_query(&json).map_err(|error| self.failure(&error)));
        self.control.clear_running();
        let page = page?;
        let _ = self.session.release_page(page.page_id);
        Ok(page)
    }

    /// `\c[onnect] [-reuse-previous=on|off] dbname`: the next statements go to `dbname`. A user, host or port after the name is
    /// not something this session can take: running the rest of the file on the wrong server is worse than stopping.
    fn connect(&mut self, arguments: &[&str]) -> CommandResult {
        let refuse = |message: &str| CommandResult::Failed(Failure::Rejected(message.to_owned()));
        let arguments: Vec<&str> = arguments.iter().copied().filter(|argument| !argument.starts_with("-reuse-previous")).collect();
        let name = match arguments.as_slice() {
            [] => return CommandResult::Skipped("\\connect without a database name is left out".to_owned()),
            [name] => unquote(name),
            _ => return refuse("\\connect with a user, host or port is not supported: the file runs on the connection it was started on"),
        };
        if name == "-" || self.database.as_deref() == Some(name.as_str()) {
            return CommandResult::Done(format!("stays on {}", self.database.as_deref().unwrap_or("the current database")));
        }
        let previous = self.database.replace(name.clone());
        match self.send("SELECT 1", true) {
            Ok(_) => CommandResult::Done(format!("switched to {name}")),
            Err(failure) => {
                self.database = previous;
                CommandResult::Failed(match failure {
                    Failure::Rejected(message) => Failure::Rejected(format!("could not connect to {name}: {message}")),
                    other => other,
                })
            }
        }
    }
}

/// The statement that puts `schema` ahead of the search path the session has (just `schema` when the path is empty: PostgreSQL shows the setting `''` as `""`). An `E` string
/// reads the same whatever `standard_conforming_strings` says.
pub(in crate::app) fn search_path_sql(schema: &str) -> String {
    let literal = schema.replace('\\', "\\\\").replace('\'', "''");
    format!("SELECT set_config('search_path', quote_ident(E'{literal}') || CASE WHEN current_setting('search_path') IN ('', '\"\"') THEN '' ELSE ', ' || current_setting('search_path') END, false)")
}

/// The first schema of a `search_path` setting (`"$user", public`).
fn first_schema(path: &str) -> Option<String> {
    let first = path.split(',').next()?.trim();
    match first.strip_prefix('"').and_then(|rest| rest.strip_suffix('"')) {
        Some(inner) => Some(inner.replace("\"\"", "\"")),
        None => Some(first.to_owned()),
    }
}

/// `"my db"` or `my_db` as the name it spells.
fn unquote(argument: &str) -> String {
    match argument.strip_prefix('"').and_then(|rest| rest.strip_suffix('"')) {
        Some(inner) => inner.replace("\"\"", "\""),
        None => argument.to_owned(),
    }
}

impl Executor for DriverExecutor {
    fn begin(&mut self) -> Result<(), Failure> {
        // MySQL and MariaDB spell it START TRANSACTION: only that is a transaction in every mode of the server.
        let sql = if matches!(self.kind, ConnectionKind::Mysql | ConnectionKind::Mariadb) { "START TRANSACTION" } else { "BEGIN" };
        self.send(sql, true).map(|_| ())
    }

    fn execute(&mut self, sql: &str) -> Result<Done, Failure> {
        let page = self.send(sql, true)?;
        // A `USE` changes the session's database: the requests that follow name it (else the driver selects the old one again). A
        // `DROP DATABASE` of the database the requests name leaves no database to select: they name none until the file `USE`s one.
        if let Some(comments) = self.mysql {
            if let Some(database) = use_target(sql.as_bytes(), comments) {
                self.database = Some(database);
            } else if let Some(dropped) = drop_database_target(sql.as_bytes(), comments) {
                if self.database.as_deref() == Some(dropped.as_str()) {
                    self.database = None;
                }
            }
        }
        let result_set = !page.columns.is_empty();
        Ok(Done { result_set, affected: if result_set { 0 } else { page.total_count.unwrap_or(0) } })
    }

    fn command(&mut self, line: &str, in_transaction: bool) -> CommandResult {
        let mut words = line.split_whitespace();
        let command = words.next().unwrap_or_default();
        if matches!(command, "\\c" | "\\connect") {
            if in_transaction {
                return CommandResult::Failed(Failure::Rejected("\\connect inside a transaction: the database cannot change under it (run the file in autocommit mode)".to_owned()));
            }
            let arguments: Vec<&str> = words.collect();
            return self.connect(&arguments);
        }
        CommandResult::Skipped(format!("{command} is a command of the psql client, not SQL: it is left out"))
    }

    fn commit(&mut self) -> Result<(), Failure> {
        self.send("COMMIT", false).map(|_| ())
    }

    fn rollback(&mut self) -> bool {
        self.send("ROLLBACK", false).is_ok()
    }
}

impl Drop for DriverExecutor {
    fn drop(&mut self) {
        let _ = self.session.disconnect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_statement_that_puts_a_schema_first_keeps_the_rest_of_the_path_and_has_no_trailing_comma_on_an_empty_one() {
        let sql = search_path_sql("o'k\\");
        assert!(sql.contains("quote_ident(E'o''k\\\\')"), "{sql}");
        assert!(sql.contains("CASE WHEN current_setting('search_path') IN ('', '\"\"') THEN '' ELSE ', ' || current_setting('search_path') END"), "{sql}");
    }

    #[test]
    fn the_first_schema_of_a_search_path_is_read_quoted_or_not() {
        assert_eq!(first_schema("\"$user\", public").as_deref(), Some("$user"));
        assert_eq!(first_schema("public").as_deref(), Some("public"));
        assert_eq!(first_schema("\"my \"\"s\"\"\" , public").as_deref(), Some("my \"s\""));
    }
}
