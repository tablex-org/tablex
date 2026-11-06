//! What each kind of server offers for its running sessions, as data (no window): the query that lists them, the columns of the list,
//! how a row of that query becomes a [`ProcessRow`], and the statements that cancel a query or end a session.
//!
//! A kind is one [`ProcessEngine`]; a server without one has no Process List. MongoDB (`currentOp` / `killOp`) and Redis
//! (`CLIENT LIST` / `CLIENT KILL`) are one more entry each: a list command, a parser and the actions.
//!
//! **Nothing typed or read from the server is spliced into SQL as text.** A session id is parsed as an unsigned integer and printed
//! as one; the only text a statement carries (ClickHouse's `query_id`) goes through the dialect's string-literal quoting.

use gpui::SharedString;

use crate::connections::ConnectionKind;
use crate::sql_account::catalog::TextRows;
use crate::sql_ddl::{dialect_for, literal};

/// What a statement acts on, as the server named it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) enum Target {
    /// A numeric session id (a PostgreSQL pid, a MySQL / MariaDB connection id).
    Number(u64),
    /// A textual id (a ClickHouse `query_id`).
    Text(String),
}

impl Target {
    /// What identifies the row between two reads of the list.
    pub(in crate::app) fn key(&self) -> String {
        match self {
            Self::Number(number) => number.to_string(),
            Self::Text(text) => text.clone(),
        }
    }
}

/// One column of the list.
pub(in crate::app) struct Column {
    pub(in crate::app) title: &'static str,
    pub(in crate::app) width: f32,
    pub(in crate::app) right: bool,
}

/// The default width of the Query column: the statement is what the user reads the list for.
const QUERY_WIDTH: f32 = 640.0;

const fn column(title: &'static str, width: f32) -> Column {
    Column { title, width, right: false }
}

const fn number(title: &'static str, width: f32) -> Column {
    Column { title, width, right: true }
}

/// What the two buttons do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum ActionKind {
    /// Stops the statement a session is running; the session stays.
    Cancel,
    /// Ends the session (its connection).
    Kill,
}

/// One action a server has.
pub(in crate::app) struct Action {
    /// The button and the dialog's title.
    pub(in crate::app) label: &'static str,
    /// The dialog's question.
    pub(in crate::app) question: &'static str,
    /// The statement for `target`; `None` when the target is not the kind of id the statement takes.
    statement: fn(&Target) -> Option<String>,
}

impl Action {
    pub(in crate::app) fn statement(&self, target: &Target) -> Option<String> {
        (self.statement)(target)
    }
}

/// One row of the list.
pub(in crate::app) struct ProcessRow {
    pub(in crate::app) target: Target,
    /// One text per column of the engine, in order.
    pub(in crate::app) cells: Vec<SharedString>,
    /// "PID 42 · app" or the like: how the dialog names the row.
    pub(in crate::app) summary: String,
    /// The statement the session runs, on one line.
    pub(in crate::app) query: SharedString,
    /// Waiting for a request: hidden by Hide idle.
    pub(in crate::app) idle: bool,
    /// The session this list is read through: cancelling or ending it would end the list.
    pub(in crate::app) own: bool,
    /// How long it has been at it (the order of the list).
    seconds: f64,
    /// User, database and query, lower-cased, for the filter.
    haystack: String,
}

impl ProcessRow {
    pub(in crate::app) fn key(&self) -> String {
        self.target.key()
    }

    /// The filter's `needle` (lower-cased) is in the user, the database or the query.
    pub(in crate::app) fn matches(&self, needle: &str) -> bool {
        needle.is_empty() || self.haystack.contains(needle)
    }
}

/// The longest query kept for the list and the filter (a statement can be megabytes).
const QUERY_LIMIT: usize = 600;

/// Everything a parser knows about a row, before it is checked and shaped.
struct Parts<'a> {
    target: Target,
    cells: Vec<String>,
    summary: String,
    user: &'a str,
    database: &'a str,
    query: &'a str,
    idle: bool,
    own: bool,
    seconds: f64,
}

impl Parts<'_> {
    fn build(self, id_column: usize, query_column: usize) -> ProcessRow {
        let query = crate::script_run::one_line(self.query, QUERY_LIMIT);
        let mut cells = self.cells;
        if self.own {
            cells[id_column].push_str(" (this app)");
        }
        cells[query_column] = query.clone();
        let haystack = format!("{}\u{1}{}\u{1}{}", self.user, self.database, query).to_lowercase();
        ProcessRow {
            target: self.target,
            cells: cells.into_iter().map(SharedString::from).collect(),
            summary: self.summary,
            query: query.into(),
            idle: self.idle,
            own: self.own,
            seconds: self.seconds,
            haystack,
        }
    }
}

/// A row of the server's answer, by position.
struct Fields<'a>(&'a [Option<String>]);

impl<'a> Fields<'a> {
    fn text(&self, at: usize) -> Result<&'a str, String> {
        self.0.get(at).map(|cell| cell.as_deref().unwrap_or("")).ok_or_else(|| format!("The server's answer has no column {}.", at + 1))
    }

    fn id(&self, at: usize) -> Result<u64, String> {
        let text = self.text(at)?;
        text.trim().parse().map_err(|_| format!("\"{text}\" is not a session id."))
    }

    fn flag(&self, at: usize) -> Result<bool, String> {
        Ok(matches!(self.text(at)?.trim(), "true" | "1" | "t"))
    }

    fn seconds(&self, at: usize) -> Result<f64, String> {
        let text = self.text(at)?.trim();
        if text.is_empty() {
            return Ok(0.0);
        }
        text.parse::<f64>().map(|seconds| if seconds.is_finite() { seconds.max(0.0) } else { 0.0 }).map_err(|_| format!("\"{text}\" is not a duration."))
    }
}

fn duration_text(seconds: f64) -> String {
    // A finite but huge number from the server (`from_secs_f64` panics past `u64::MAX` seconds) is the longest there is.
    crate::script_run::format_duration(std::time::Duration::try_from_secs_f64(seconds).unwrap_or(std::time::Duration::MAX))
}

/// A kind of server that has a Process List.
pub(in crate::app) struct ProcessEngine {
    pub(in crate::app) label: &'static str,
    /// Sent as it is on the tab's own session.
    pub(in crate::app) list: &'static str,
    pub(in crate::app) columns: &'static [Column],
    parse: fn(&Fields) -> Result<ProcessRow, String>,
    pub(in crate::app) cancel: Option<Action>,
    pub(in crate::app) kill: Option<Action>,
}

impl ProcessEngine {
    pub(in crate::app) fn of(kind: ConnectionKind) -> Option<&'static Self> {
        match kind {
            ConnectionKind::Postgres => Some(&POSTGRES),
            ConnectionKind::Mysql => Some(&MYSQL),
            ConnectionKind::Mariadb => Some(&MARIADB),
            ConnectionKind::Clickhouse => Some(&CLICKHOUSE),
            ConnectionKind::Sqlite | ConnectionKind::Mongodb | ConnectionKind::Redis => None,
        }
    }

    pub(in crate::app) fn action(&self, kind: ActionKind) -> Option<&Action> {
        match kind {
            ActionKind::Cancel => self.cancel.as_ref(),
            ActionKind::Kill => self.kill.as_ref(),
        }
    }

    /// The rows of the server's answer: the sessions running something first, the longest at it first.
    pub(in crate::app) fn rows(&self, answer: &TextRows) -> Result<Vec<ProcessRow>, String> {
        let mut rows = answer.iter().map(|row| (self.parse)(&Fields(row))).collect::<Result<Vec<_>, _>>()?;
        rows.sort_by(|a, b| a.idle.cmp(&b.idle).then(b.seconds.total_cmp(&a.seconds)).then_with(|| a.key().cmp(&b.key())));
        Ok(rows)
    }
}

/// The rows to show: the idle ones only when `hide_idle` is off, and those that match the filter text.
pub(in crate::app) fn shown(rows: &[ProcessRow], hide_idle: bool, filter: &str) -> Vec<usize> {
    let needle = filter.trim().to_lowercase();
    rows.iter().enumerate().filter(|(_, row)| !(hide_idle && row.idle) && row.matches(&needle)).map(|(at, _)| at).collect()
}

// ---- PostgreSQL ---------------------------------------------------------------------------------------------------------------------------

/// `pg_stat_activity` of the client backends (the server's own workers are not sessions to kill). Every column is text, so what is
/// shown is what the server says; the duration is seconds since the statement began, computed by the server.
const POSTGRES_LIST: &str = "SELECT pid::text, COALESCE(usename::text, ''), COALESCE(datname::text, ''), COALESCE(application_name, ''), \
     COALESCE(client_addr::text, ''), COALESCE(state, ''), COALESCE(wait_event_type, ''), COALESCE(wait_event, ''), \
     COALESCE(to_char(backend_start, 'YYYY-MM-DD HH24:MI:SS'), ''), COALESCE(to_char(xact_start, 'YYYY-MM-DD HH24:MI:SS'), ''), \
     COALESCE(to_char(query_start, 'YYYY-MM-DD HH24:MI:SS'), ''), \
     COALESCE(round(EXTRACT(EPOCH FROM (clock_timestamp() - query_start))::numeric, 3)::text, ''), \
     (pid = pg_backend_pid())::text, COALESCE(query, '') \
     FROM pg_catalog.pg_stat_activity WHERE backend_type = 'client backend' ORDER BY pid";

/// The ones that say most come first (a narrow window shows PID to Query); the rest follow.
static POSTGRES_COLUMNS: [Column; 12] = [
    number("PID", 70.0),
    column("User", 110.0),
    column("Database", 120.0),
    column("State", 100.0),
    number("Duration", 90.0),
    column("Query", QUERY_WIDTH),
    column("Application", 130.0),
    column("Client", 110.0),
    column("Wait", 160.0),
    column("Backend start", 150.0),
    column("Transaction start", 150.0),
    column("Query start", 150.0),
];

fn postgres_row(row: &Fields) -> Result<ProcessRow, String> {
    let pid = row.id(0)?;
    let (user, database, state) = (row.text(1)?, row.text(2)?, row.text(5)?);
    let (wait_type, wait_event) = (row.text(6)?, row.text(7)?);
    let seconds = row.seconds(11)?;
    let wait = match (wait_type, wait_event) {
        ("", "") => String::new(),
        (kind, "") => kind.to_owned(),
        (kind, event) => format!("{kind}: {event}"),
    };
    let cells = vec![
        pid.to_string(),
        user.to_owned(),
        database.to_owned(),
        state.to_owned(),
        if row.text(10)?.is_empty() { String::new() } else { duration_text(seconds) },
        String::new(),
        row.text(3)?.to_owned(),
        row.text(4)?.to_owned(),
        wait,
        row.text(8)?.to_owned(),
        row.text(9)?.to_owned(),
        row.text(10)?.to_owned(),
    ];
    Ok(Parts {
        target: Target::Number(pid),
        cells,
        summary: format!("PID {pid} \u{00B7} {}", if user.is_empty() { "no user" } else { user }),
        user,
        database,
        query: row.text(13)?,
        idle: state == "idle",
        own: row.flag(12)?,
        seconds,
    }
    .build(0, 5))
}

fn postgres_cancel(target: &Target) -> Option<String> {
    match target {
        Target::Number(pid) => Some(format!("SELECT pg_cancel_backend({pid})")),
        Target::Text(_) => None,
    }
}

fn postgres_kill(target: &Target) -> Option<String> {
    match target {
        Target::Number(pid) => Some(format!("SELECT pg_terminate_backend({pid})")),
        Target::Text(_) => None,
    }
}

static POSTGRES: ProcessEngine = ProcessEngine {
    label: "PostgreSQL",
    list: POSTGRES_LIST,
    columns: &POSTGRES_COLUMNS,
    parse: postgres_row,
    cancel: Some(Action { label: "Cancel Query", question: "Cancel this query?", statement: postgres_cancel }),
    kill: Some(Action { label: "Kill Session", question: "Kill this session?", statement: postgres_kill }),
};

// ---- MySQL and MariaDB ----------------------------------------------------------------------------------------------------------------

/// `information_schema.PROCESSLIST`. An account without the PROCESS privilege sees only its own sessions.
const MYSQL_LIST: &str = "SELECT CAST(ID AS CHAR), COALESCE(USER, ''), COALESCE(HOST, ''), COALESCE(DB, ''), COALESCE(COMMAND, ''), \
     CAST(COALESCE(TIME, 0) AS CHAR), COALESCE(STATE, ''), COALESCE(INFO, ''), CAST(ID = CONNECTION_ID() AS CHAR) \
     FROM information_schema.PROCESSLIST ORDER BY ID";

static MYSQL_COLUMNS: [Column; 8] = [
    number("ID", 80.0),
    column("User", 110.0),
    column("Database", 120.0),
    column("Command", 90.0),
    column("State", 140.0),
    number("Time", 90.0),
    column("Query", QUERY_WIDTH),
    column("Host", 150.0),
];

fn mysql_row(row: &Fields) -> Result<ProcessRow, String> {
    let id = row.id(0)?;
    let (user, database, command) = (row.text(1)?, row.text(3)?, row.text(4)?);
    let seconds = row.seconds(5)?;
    let cells = vec![
        id.to_string(),
        user.to_owned(),
        database.to_owned(),
        command.to_owned(),
        row.text(6)?.to_owned(),
        duration_text(seconds),
        String::new(),
        row.text(2)?.to_owned(),
    ];
    Ok(Parts {
        target: Target::Number(id),
        cells,
        summary: format!("Connection {id} \u{00B7} {}", if user.is_empty() { "no user" } else { user }),
        user,
        database,
        query: row.text(7)?,
        idle: command == "Sleep",
        own: row.flag(8)?,
        seconds,
    }
    .build(0, 6))
}

fn mysql_cancel(target: &Target) -> Option<String> {
    match target {
        Target::Number(id) => Some(format!("KILL QUERY {id}")),
        Target::Text(_) => None,
    }
}

fn mysql_kill(target: &Target) -> Option<String> {
    match target {
        Target::Number(id) => Some(format!("KILL {id}")),
        Target::Text(_) => None,
    }
}

static MYSQL: ProcessEngine = ProcessEngine {
    label: "MySQL",
    list: MYSQL_LIST,
    columns: &MYSQL_COLUMNS,
    parse: mysql_row,
    cancel: Some(Action { label: "Cancel Query", question: "Cancel this query?", statement: mysql_cancel }),
    kill: Some(Action { label: "Kill Session", question: "Kill this connection?", statement: mysql_kill }),
};

static MARIADB: ProcessEngine = ProcessEngine {
    label: "MariaDB",
    list: MYSQL_LIST,
    columns: &MYSQL_COLUMNS,
    parse: mysql_row,
    cancel: Some(Action { label: "Cancel Query", question: "Cancel this query?", statement: mysql_cancel }),
    kill: Some(Action { label: "Kill Session", question: "Kill this connection?", statement: mysql_kill }),
};

// ---- ClickHouse -----------------------------------------------------------------------------------------------------------------------------

/// `system.processes`: the queries running now (ClickHouse has no session to list). `queryID()` is the id of this very query.
const CLICKHOUSE_LIST: &str = "SELECT query_id, user, toString(address), toString(round(elapsed, 3)), toString(read_rows), toString(memory_usage), \
     query, toString(query_id = queryID()) FROM system.processes ORDER BY elapsed DESC";

static CLICKHOUSE_COLUMNS: [Column; 7] = [
    column("Query ID", 290.0),
    column("User", 110.0),
    number("Elapsed", 90.0),
    column("Query", QUERY_WIDTH),
    column("Address", 150.0),
    number("Read rows", 100.0),
    number("Memory", 90.0),
];

fn clickhouse_row(row: &Fields) -> Result<ProcessRow, String> {
    let id = row.text(0)?;
    if id.is_empty() {
        return Err("A query has no id.".to_owned());
    }
    let user = row.text(1)?;
    let seconds = row.seconds(3)?;
    let rows: u64 = row.text(4)?.trim().parse().unwrap_or(0);
    let memory: u64 = row.text(5)?.trim().parse().unwrap_or(0);
    let cells = vec![
        id.to_owned(),
        user.to_owned(),
        duration_text(seconds),
        String::new(),
        row.text(2)?.to_owned(),
        rows.to_string(),
        crate::binary_value::format_size(usize::try_from(memory).unwrap_or(usize::MAX)),
        String::new(),
    ];
    Ok(Parts {
        target: Target::Text(id.to_owned()),
        cells,
        summary: format!("Query {id} \u{00B7} {}", if user.is_empty() { "no user" } else { user }),
        user,
        database: "",
        query: row.text(6)?,
        idle: false,
        own: row.flag(7)?,
        seconds,
    }
    .build(0, 3))
}

/// `KILL QUERY WHERE query_id = '…'`: the id is a string literal of the dialect, never text in the statement.
fn clickhouse_cancel(target: &Target) -> Option<String> {
    match target {
        Target::Text(id) => Some(format!("KILL QUERY WHERE query_id = {}", literal(dialect_for(ConnectionKind::Clickhouse)?, id))),
        Target::Number(_) => None,
    }
}

static CLICKHOUSE: ProcessEngine = ProcessEngine {
    label: "ClickHouse",
    list: CLICKHOUSE_LIST,
    columns: &CLICKHOUSE_COLUMNS,
    parse: clickhouse_row,
    cancel: Some(Action { label: "Cancel Query", question: "Cancel this query?", statement: clickhouse_cancel }),
    kill: None,
};

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(rows: &[&[&str]]) -> TextRows {
        rows.iter().map(|row| row.iter().map(|cell| Some((*cell).to_owned())).collect()).collect()
    }

    fn pg_row<'a>(pid: &'a str, state: &'a str, seconds: &'a str, own: &'a str, query: &'a str) -> [&'a str; 14] {
        [pid, "app", "warehouse", "psql", "10.0.0.5", state, "Lock", "relation", "2026-11-06 09:00:00", "", "2026-11-06 09:01:00", seconds, own, query]
    }

    #[test]
    fn every_server_with_sessions_has_an_engine_and_the_others_do_not() {
        for kind in [ConnectionKind::Postgres, ConnectionKind::Mysql, ConnectionKind::Mariadb, ConnectionKind::Clickhouse] {
            assert!(ProcessEngine::of(kind).is_some(), "{kind:?}");
        }
        for kind in [ConnectionKind::Sqlite, ConnectionKind::Mongodb, ConnectionKind::Redis] {
            assert!(ProcessEngine::of(kind).is_none(), "{kind:?}");
        }
    }

    #[test]
    fn statements_per_dialect_are_the_ones_the_servers_take() {
        let pg = ProcessEngine::of(ConnectionKind::Postgres).unwrap();
        let pid = Target::Number(4242);
        assert_eq!(pg.cancel.as_ref().unwrap().statement(&pid).as_deref(), Some("SELECT pg_cancel_backend(4242)"));
        assert_eq!(pg.kill.as_ref().unwrap().statement(&pid).as_deref(), Some("SELECT pg_terminate_backend(4242)"));
        for kind in [ConnectionKind::Mysql, ConnectionKind::Mariadb] {
            let engine = ProcessEngine::of(kind).unwrap();
            let id = Target::Number(18_446_744_073_709_551_615);
            assert_eq!(engine.cancel.as_ref().unwrap().statement(&id).as_deref(), Some("KILL QUERY 18446744073709551615"));
            assert_eq!(engine.kill.as_ref().unwrap().statement(&id).as_deref(), Some("KILL 18446744073709551615"));
        }
        let ch = ProcessEngine::of(ConnectionKind::Clickhouse).unwrap();
        assert!(ch.kill.is_none(), "ClickHouse has no session to end");
        assert_eq!(
            ch.cancel.as_ref().unwrap().statement(&Target::Text("a1-b2".into())).as_deref(),
            Some("KILL QUERY WHERE query_id = 'a1-b2'")
        );
    }

    #[test]
    fn a_text_id_never_leaves_its_string_literal_and_a_wrong_kind_of_id_has_no_statement() {
        let ch = ProcessEngine::of(ConnectionKind::Clickhouse).unwrap();
        let hostile = Target::Text("x' OR 1=1; DROP TABLE t; -- \\".into());
        let sql = ch.cancel.as_ref().unwrap().statement(&hostile).unwrap();
        // The quote is doubled and the backslash escaped (ClickHouse reads one as an escape): one literal, to the end of the statement.
        assert_eq!(sql, "KILL QUERY WHERE query_id = 'x'' OR 1=1; DROP TABLE t; -- \\\\'");
        let pg = ProcessEngine::of(ConnectionKind::Postgres).unwrap();
        assert_eq!(pg.cancel.as_ref().unwrap().statement(&hostile), None);
        assert_eq!(ch.cancel.as_ref().unwrap().statement(&Target::Number(1)), None);
    }

    #[test]
    fn postgres_rows_are_shaped_marked_ordered_and_filtered() {
        let pg = ProcessEngine::of(ConnectionKind::Postgres).unwrap();
        let rows = pg
            .rows(&answer(&[
                &pg_row("10", "idle", "900.5", "false", "SELECT 1"),
                &pg_row("11", "active", "3.25", "false", "SELECT\n  pg_sleep(30)"),
                &pg_row("12", "active", "61.0", "true", "SELECT pid FROM pg_stat_activity"),
            ]))
            .unwrap();
        // Running first, the longest at it first; idle last.
        assert_eq!(rows.iter().map(ProcessRow::key).collect::<Vec<_>>(), ["12", "11", "10"]);
        assert!(rows[0].own && !rows[1].own);
        assert_eq!(rows[0].cells[0].as_ref(), "12 (this app)");
        assert_eq!(rows[1].cells[8].as_ref(), "Lock: relation");
        assert_eq!(rows[1].cells[4].as_ref(), "3.25 s");
        assert_eq!(rows[1].query.as_ref(), "SELECT pg_sleep(30)", "a query is one line");
        assert_eq!(rows[1].target, Target::Number(11));
        assert!(rows[2].idle);
        // Hide idle, and the filter over the user, the database and the query (any case).
        assert_eq!(shown(&rows, true, ""), [0, 1]);
        assert_eq!(shown(&rows, false, ""), [0, 1, 2]);
        assert_eq!(shown(&rows, false, "  PG_SLEEP "), [1]);
        assert_eq!(shown(&rows, false, "warehouse"), [0, 1, 2]);
        assert_eq!(shown(&rows, false, "nobody"), Vec::<usize>::new());
        assert_eq!(shown(&rows, true, "select 1"), Vec::<usize>::new(), "the idle one stays hidden");
    }

    #[test]
    fn mysql_and_clickhouse_rows_are_shaped() {
        let mysql = ProcessEngine::of(ConnectionKind::Mysql).unwrap();
        let rows = mysql
            .rows(&answer(&[
                &["5", "root", "localhost:3306", "shop", "Query", "0", "executing", "SELECT SLEEP(30)", "0"],
                &["6", "app", "10.0.0.2:1", "", "Sleep", "120", "", "", "1"],
            ]))
            .unwrap();
        assert_eq!((rows[0].key(), rows[0].idle, rows[0].own), ("5".to_owned(), false, false));
        assert_eq!((rows[1].key(), rows[1].idle, rows[1].own), ("6".to_owned(), true, true));
        assert_eq!(rows[1].cells[5].as_ref(), "2 min 0 s");
        let ch = ProcessEngine::of(ConnectionKind::Clickhouse).unwrap();
        let rows = ch.rows(&answer(&[&["q-1", "tablex", "::ffff:172.17.0.1", "0.125", "1000", "1048576", "SELECT sleep(3)", "true"]])).unwrap();
        assert_eq!(rows[0].target, Target::Text("q-1".into()));
        assert!(rows[0].own && !rows[0].idle);
        assert_eq!(rows[0].cells[2].as_ref(), "125 ms");
        assert_eq!(rows[0].cells[5].as_ref(), "1000");
    }

    #[test]
    fn a_row_that_is_not_what_the_query_asks_for_is_an_error_and_never_a_statement() {
        let pg = ProcessEngine::of(ConnectionKind::Postgres).unwrap();
        let bad_id = pg.rows(&answer(&[&pg_row("1; DROP TABLE x", "active", "1", "false", "q")]));
        assert!(bad_id.err().is_some_and(|error| error.contains("not a session id")));
        let short = pg.rows(&answer(&[&["1", "app"]]));
        assert!(short.is_err());
        let ch = ProcessEngine::of(ConnectionKind::Clickhouse).unwrap();
        assert!(ch.rows(&answer(&[&["", "u", "a", "0", "0", "0", "q", "false"]])).is_err());
    }

    #[test]
    fn a_huge_but_finite_duration_does_not_panic() {
        let pg = ProcessEngine::of(ConnectionKind::Postgres).unwrap();
        let rows = pg.rows(&answer(&[&pg_row("1", "active", "1e300", "false", "q")])).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].cells[4].is_empty());
    }

    #[test]
    fn the_columns_that_say_most_come_first_and_the_query_is_wide() {
        let titles = |kind| ProcessEngine::of(kind).unwrap().columns.iter().map(|column| column.title).collect::<Vec<_>>();
        assert_eq!(titles(ConnectionKind::Postgres)[..6], ["PID", "User", "Database", "State", "Duration", "Query"]);
        assert_eq!(titles(ConnectionKind::Mysql)[..7], ["ID", "User", "Database", "Command", "State", "Time", "Query"]);
        assert_eq!(titles(ConnectionKind::Mariadb)[7], "Host");
        assert_eq!(titles(ConnectionKind::Clickhouse)[..4], ["Query ID", "User", "Elapsed", "Query"]);
        for kind in [ConnectionKind::Postgres, ConnectionKind::Mysql, ConnectionKind::Mariadb, ConnectionKind::Clickhouse] {
            let engine = ProcessEngine::of(kind).unwrap();
            let query = engine.columns.iter().find(|column| column.title == "Query").unwrap();
            assert!(query.width >= 600.0);
        }
    }

    #[test]
    fn a_huge_query_is_cut_for_the_list_and_the_filter() {
        let pg = ProcessEngine::of(ConnectionKind::Postgres).unwrap();
        let long = format!("SELECT {}", "x, ".repeat(100_000));
        let rows = pg.rows(&answer(&[&pg_row("1", "active", "1", "false", &long)])).unwrap();
        assert!(rows[0].query.chars().count() <= QUERY_LIMIT + 2);
        assert!(rows[0].query.ends_with('\u{2026}'));
    }
}
