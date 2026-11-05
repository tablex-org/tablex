//! Reading the source of a transfer: a query, one page at a time, over a session of its own.
//!
//! The driver's query route pages a `SELECT` (`LIMIT n+1 OFFSET m` on the server, the extra row telling whether
//! another page follows), and an Arrow page is the whole of what one call returns, so memory is one page, whatever
//! the table. The size of a page follows the data: it starts at [`FIRST_PAGE_ROWS`] and the next is sized so a page
//! is about [`PAGE_TARGET_BYTES`] of Arrow, so a table of narrow rows reads in few calls and one of large documents
//! never piles up thousands of them.
//!
//! **Paging by offset costs more with every page**: the server walks past the rows it skips, so a table read this way takes
//! time that grows with the square of its size (1,000,000 rows to a file in 2 s, 5,000,000 in 67 s). A table with a
//! primary key is therefore read by *keyset* instead: each page asks for the rows after the last key of the page before
//! (`WHERE (k) > (last) ORDER BY k`), which the key's index answers in time that does not depend on how far the read is.
//! The driver offers nothing beyond its paged query (no cursor, no streaming export), so this is the same route with another
//! statement. A query typed by the user, a table without a primary key and a key that has no safe literal (a float) are paged
//! by offset, as before.
//!
//! The reader is the one place that talks to the driver for a source; [`PageFeed`] is what the rest of the transfer
//! sees, so everything above it is tested without a server.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use arrow_array::RecordBatch;
use engine_driver_api::sql::codes;
use engine_driver_api::{CancelRequest, PageColumn, QueryRequest};
use engine_driver_host::{DriverHandle, DriverSession, ImportedArrowBatch, LoaderError};

use super::super::data_import::run::{RowSource, SourceFailure};
use super::super::results::column_type::base_type_head;
use super::super::results::copy::{sql_literal, SqlTarget};
use super::super::structure_edit::apply::{ApplyControl, SessionSpec};
use crate::grid_arrow::{ArrowCellReader, BinaryDisplay};
use super::order::RowOrder;
use crate::connections::ConnectionKind;
use crate::import::source::{BadRow, Read, Row, RowProblem};

/// Rows asked for by the first page.
pub(in crate::app) const FIRST_PAGE_ROWS: u64 = 2_000;
/// The next page is sized to hold about this much Arrow data.
pub(in crate::app) const PAGE_TARGET_BYTES: usize = 24 * 1024 * 1024;
/// A page is never smaller than this (a table of huge cells still moves) nor larger than the most a driver gives.
const MIN_PAGE_ROWS: u64 = 16;
const MAX_PAGE_ROWS: u64 = 25_000;

/// What is read: one `SELECT`, on a database of the connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) struct Query {
    pub(in crate::app) database: Option<String>,
    /// The statement as it is paged by offset (a table: with its `ORDER BY` key).
    pub(in crate::app) sql: String,
    /// How to page it by key instead, when the table allows.
    pub(in crate::app) keyset: Option<Keyset>,
    /// How well the pages follow each other.
    pub(in crate::app) order: RowOrder,
}

impl Query {
    pub(in crate::app) fn new(database: Option<String>, sql: String) -> Self {
        Self { database, sql, keyset: None, order: RowOrder::Own }
    }
}

/// A table read in the order of its primary key, each page after the last key of the one before.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) struct Keyset {
    /// `SELECT * FROM <table>`.
    pub(in crate::app) select: String,
    /// The key columns in key order, quoted for the source, with their types.
    pub(in crate::app) columns: Vec<KeyColumn>,
    /// The dialect the key values are written in (until the session that reads says its own).
    pub(in crate::app) target: SqlTarget,
    /// MySQL and MariaDB do not use an index for a row comparison: `(a, b) > (x, y)` is written `a > x OR (a = x AND b > y)`.
    pub(in crate::app) expand_rows: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) struct KeyColumn {
    pub(in crate::app) name: String,
    pub(in crate::app) quoted: String,
    pub(in crate::app) data_type: String,
}

/// Whether a key column of `data_type` can be compared to a literal the way the server orders it. An explicit list per dialect:
/// an enum or set (ordered by position, compared as text), `bit`, a float (a literal is not the same number), bytes, a flag
/// and a column of no declared type (SQLite compares by what each value happens to be) are not on it.
pub(in crate::app) fn key_type_allowed(kind: ConnectionKind, data_type: &str) -> bool {
    let head = base_type_head(data_type);
    let head = head.as_str();
    match kind {
        ConnectionKind::Postgres => {
            matches!(head, "smallint" | "integer" | "int" | "bigint" | "int2" | "int4" | "int8" | "numeric" | "decimal" | "text" | "varchar" | "character" | "char" | "bpchar" | "uuid" | "date" | "timestamp" | "timestamptz")
        }
        ConnectionKind::Mysql | ConnectionKind::Mariadb => {
            matches!(head, "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint" | "decimal" | "numeric" | "char" | "varchar" | "date" | "datetime" | "timestamp")
        }
        // The affinity rules: a declared type with INT in it is an integer column, one with CHAR, CLOB or TEXT a text one.
        ConnectionKind::Sqlite => head.contains("int") || head.contains("char") || head.contains("clob") || head.contains("text"),
        // A ClickHouse key is a sorting key: several rows can share it.
        ConnectionKind::Clickhouse | ConnectionKind::Mongodb | ConnectionKind::Redis => false,
    }
}

impl Keyset {
    /// The keyset of `select` ordered by `key` (name and type of each key column, in key order), when the key is unique and every
    /// column is on the list of types that can be compared safely through a literal. `None`: page by offset.
    pub(in crate::app) fn for_table(select: &str, key: &[(String, String)], unique_key: bool, target: SqlTarget, kind: ConnectionKind) -> Option<Self> {
        if key.is_empty() || !unique_key || !key.iter().all(|(_, data_type)| key_type_allowed(kind, data_type)) {
            return None;
        }
        let columns = key
            .iter()
            .map(|(name, data_type)| Some(KeyColumn { name: name.clone(), quoted: target.quote(name).ok()?, data_type: data_type.clone() }))
            .collect::<Option<Vec<_>>>()?;
        Some(Self { select: select.to_owned(), columns, target, expand_rows: matches!(kind, ConnectionKind::Mysql | ConnectionKind::Mariadb) })
    }

    fn order(&self) -> String {
        self.columns.iter().map(|column| column.quoted.as_str()).collect::<Vec<_>>().join(", ")
    }

    /// The statement of the first page.
    pub(in crate::app) fn first(&self) -> String {
        format!("{} ORDER BY {}", self.select, self.order())
    }

    /// The statement of the page after the row whose key is `after`, its literals written in `target`'s dialect.
    fn after(&self, after: &[String], target: SqlTarget) -> Result<String, ReadError> {
        let literals = self
            .columns
            .iter()
            .zip(after)
            .map(|(column, text)| sql_literal(Some(text), &column.data_type, target).map_err(ReadError::Refused))
            .collect::<Result<Vec<_>, _>>()?;
        let condition = match self.columns.as_slice() {
            [only] => format!("{} > {}", only.quoted, literals[0]),
            _ if self.expand_rows => {
                // `a > x OR (a = x AND b > y) OR (a = x AND b = y AND c > z)`: each branch is a range of the index.
                let branches: Vec<String> = (0..self.columns.len())
                    .map(|last| {
                        let equal = (0..last).map(|index| format!("{} = {}", self.columns[index].quoted, literals[index]));
                        let greater = std::iter::once(format!("{} > {}", self.columns[last].quoted, literals[last]));
                        let terms: Vec<String> = equal.chain(greater).collect();
                        if terms.len() == 1 { terms.concat() } else { format!("({})", terms.join(" AND ")) }
                    })
                    .collect();
                branches.join(" OR ")
            }
            _ => format!("({}) > ({})", self.order(), literals.join(", ")),
        };
        Ok(format!("{} WHERE {condition} ORDER BY {}", self.select, self.order()))
    }

    /// Whether a key column of the page came as bytes (a text key the driver could not read as text).
    fn arrives_as_bytes(&self, batch: &RecordBatch, columns: &[PageColumn]) -> bool {
        self.columns.iter().any(|key| {
            columns
                .iter()
                .position(|column| column.name == key.name)
                .or_else(|| columns.iter().position(|column| column.name.eq_ignore_ascii_case(&key.name)))
                .is_some_and(|index| holds_bytes(batch.column(index).as_ref()))
        })
    }

    /// The key of row `row` of a page: the text of each key column. `None` when a key cell is NULL or the column is missing.
    fn key_of(&self, batch: &RecordBatch, columns: &[PageColumn], row: usize) -> Option<Vec<String>> {
        let mut buffer = String::new();
        self.columns
            .iter()
            .map(|key| {
                let index = columns.iter().position(|column| column.name == key.name).or_else(|| columns.iter().position(|column| column.name.eq_ignore_ascii_case(&key.name)))?;
                let reader = ArrowCellReader::new(batch.column(index).as_ref(), BinaryDisplay::Hex);
                (!reader.is_null(row)).then(|| reader.text(row, &mut buffer).to_owned())
            })
            .collect()
    }
}

/// One page of a result.
pub(in crate::app) struct Page {
    /// The Arrow data. The driver's memory stays pinned while this lives.
    pub(in crate::app) batch: RecordBatch,
    /// The columns as the driver names and types them.
    pub(in crate::app) columns: Vec<PageColumn>,
    /// Another page follows.
    pub(in crate::app) more: bool,
    /// Keeps the driver's buffers alive.
    _lease: Option<ImportedArrowBatch>,
}

#[cfg(test)]
impl Page {
    pub(in crate::app) fn new(batch: RecordBatch, columns: Vec<PageColumn>, more: bool) -> Self {
        Self { batch, columns, more, _lease: None }
    }
}

/// Why a page could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) enum ReadError {
    Cancelled,
    /// The server refused the query (a syntax error, a missing table, a statement that cannot be paged).
    Refused(String),
    /// The session cannot go on.
    Lost(String),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => f.write_str("cancelled"),
            Self::Refused(message) | Self::Lost(message) => f.write_str(message),
        }
    }
}

impl From<ReadError> for SourceFailure {
    fn from(error: ReadError) -> Self {
        match error {
            ReadError::Cancelled => Self::Cancelled,
            other => Self::Failed(other.to_string()),
        }
    }
}

/// Where the pages come from.
pub(in crate::app) trait PageFeed {
    /// The page of `limit` rows (at most) after the first `offset` rows of `query`.
    fn fetch(&mut self, query: &Query, offset: u64, limit: u64) -> Result<Page, ReadError>;

    /// The dialect the session that reads speaks (it can differ from the one the connection was saved with, as MySQL's
    /// `NO_BACKSLASH_ESCAPES` does). `None`: the one the query was made for.
    fn dialect(&self) -> Option<SqlTarget> {
        None
    }

    /// Called before the first page of a table (or query): where the dialect has one, opens the transaction its pages are read
    /// in, so the table does not change between one page and the next. One snapshot *per table*: a snapshot held across the
    /// whole run would keep its read locks and the server's old row versions for as long as the run lasts.
    fn begin_table(&mut self) -> SnapshotState {
        SnapshotState::Unavailable("not asked for")
    }

    /// Called after the last page of a table, whatever became of it: ends what `begin_table` began.
    fn end_table(&mut self) {}

    /// What the session could not set up (a time zone the server ignored): said in the summary of every table.
    fn notes(&self) -> Vec<String> {
        Vec::new()
    }
}

/// Whether the pages of a read come from one snapshot, so the tables do not change between one page and the next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) enum SnapshotState {
    /// One transaction holds the snapshot of the table from its first page to its last.
    Held,
    /// Every page sees the database as it is then; why not.
    Unavailable(&'static str),
}

/// The size of the page after one of `rows` rows and `bytes` of Arrow data, asked for as `limit`.
pub(in crate::app) fn next_limit(limit: u64, rows: usize, bytes: usize) -> u64 {
    if rows == 0 {
        return limit;
    }
    let per_row = (bytes / rows).max(1);
    ((PAGE_TARGET_BYTES / per_row) as u64).clamp(MIN_PAGE_ROWS, MAX_PAGE_ROWS)
}

/// Whether a column of the page holds bytes.
pub(in crate::app) fn holds_bytes(array: &dyn arrow_array::Array) -> bool {
    use arrow_schema::DataType;
    matches!(array.data_type(), DataType::Binary | DataType::LargeBinary | DataType::BinaryView | DataType::FixedSizeBinary(_))
}

/// The rows of a page as the text the grid shows (`None` for a SQL NULL); `first` is the number of its first row. A row whose
/// cell in one of the `text_columns` (page column index and name: columns the server calls text) came as bytes is a bad row:
/// bytes that are not valid text cannot be written to a text column as they are.
pub(in crate::app) fn page_rows(batch: &RecordBatch, first: u64, text_columns: &[(usize, String)]) -> VecDeque<Read> {
    let readers: Vec<ArrowCellReader<'_>> = batch.columns().iter().map(|array| ArrowCellReader::new(array.as_ref(), BinaryDisplay::Hex)).collect();
    let watched: Vec<&(usize, String)> = text_columns.iter().filter(|(index, _)| batch.columns().get(*index).is_some_and(|array| holds_bytes(array.as_ref()))).collect();
    let mut buffer = String::new();
    (0..batch.num_rows())
        .map(|row| {
            let number = first + row as u64 + 1;
            if let Some((_, name)) = watched.iter().find(|(index, _)| !readers[*index].is_null(row)) {
                return Read::Bad(BadRow { number, line: 0, problem: RowProblem::BytesInText(name.clone()) });
            }
            Read::Row(Row {
                number,
                line: 0,
                cells: readers.iter().map(|reader| (!reader.is_null(row)).then(|| reader.text(row, &mut buffer).to_owned())).collect(),
                extra: Vec::new(),
            })
        })
        .collect()
}

/// Where a read stands: the pages of one query, one after the other, sized to the data.
pub(in crate::app) struct Cursor {
    query: Query,
    /// Rows read so far.
    read: u64,
    limit: u64,
    /// The key of the last row of the last page (a keyset read).
    after: Option<Vec<String>>,
    started: bool,
    more: bool,
    /// A key arrived as bytes: the rest is read by offset, in the order of the same key.
    by_offset: bool,
}

impl Cursor {
    pub(in crate::app) fn new(query: Query) -> Self {
        Self { query, read: 0, limit: FIRST_PAGE_ROWS, after: None, started: false, more: true, by_offset: false }
    }

    /// Rows read before the next page.
    pub(in crate::app) fn read(&self) -> u64 {
        self.read
    }

    /// Whether another page may follow (the first is always asked for).
    pub(in crate::app) fn more(&self) -> bool {
        !self.started || self.more
    }

    /// The next page, or `None` when the last one was the last.
    pub(in crate::app) fn next<F: PageFeed>(&mut self, feed: &mut F) -> Result<Option<Page>, ReadError> {
        if !self.more() {
            return Ok(None);
        }
        let keyset = if self.by_offset { None } else { self.query.keyset.clone() };
        let page = match (&keyset, &self.after) {
            (Some(keyset), Some(after)) => {
                let target = feed.dialect().unwrap_or(keyset.target);
                let query = Query { database: self.query.database.clone(), sql: keyset.after(after, target)?, keyset: None, order: RowOrder::Key };
                feed.fetch(&query, 0, self.limit)?
            }
            (Some(keyset), None) => {
                let query = Query { database: self.query.database.clone(), sql: keyset.first(), keyset: None, order: RowOrder::Key };
                feed.fetch(&query, 0, self.limit)?
            }
            (None, _) => feed.fetch(&self.query, self.read, self.limit)?,
        };
        let rows = page.batch.num_rows();
        if let (Some(keyset), true) = (&keyset, rows > 0) {
            if keyset.arrives_as_bytes(&page.batch, &page.columns) {
                // Text the driver could not give as text (invalid UTF-8) would be written as `0x…` and compared as a string: the
                // key is the same, so the pages go on by offset in its order.
                self.by_offset = true;
            } else {
                self.after = Some(
                    keyset
                        .key_of(&page.batch, &page.columns, rows - 1)
                        .ok_or_else(|| ReadError::Refused("A primary key value is NULL or missing from the result: the table cannot be read by key.".to_owned()))?,
                );
            }
        }
        self.read += rows as u64;
        self.more = page.more && rows > 0;
        self.started = true;
        self.limit = next_limit(self.limit, rows, page.batch.get_array_memory_size());
        Ok(Some(page))
    }
}

/// The rows of a query, one at a time, for the load.
pub(in crate::app) struct QueryRows<'a, F: PageFeed> {
    feed: &'a mut F,
    cursor: Cursor,
    started: bool,
    rows: VecDeque<Read>,
    columns: Vec<PageColumn>,
    /// Columns (by name) the server calls text: bytes in them are refused row by row.
    text_columns: Vec<String>,
    /// The same, by index in the page.
    watched: Vec<(usize, String)>,
    /// Bytes of text read, shared with the progress counters.
    bytes: Arc<AtomicU64>,
}

impl<'a, F: PageFeed> QueryRows<'a, F> {
    pub(in crate::app) fn new(feed: &'a mut F, query: Query, bytes: Arc<AtomicU64>) -> Self {
        Self { feed, cursor: Cursor::new(query), started: false, rows: VecDeque::new(), columns: Vec::new(), text_columns: Vec::new(), watched: Vec::new(), bytes }
    }

    /// Bytes in these columns (they are written to text columns) make the row a bad one. Call before the first page.
    pub(in crate::app) fn refuse_bytes_in(&mut self, columns: Vec<String>) {
        self.text_columns = columns;
    }

    fn fetch(&mut self) -> Result<(), ReadError> {
        let first = self.cursor.read();
        let Some(page) = self.cursor.next(self.feed)? else {
            self.rows.clear();
            return Ok(());
        };
        if !self.started {
            self.watched = self.text_columns.iter().filter_map(|name| page.columns.iter().position(|column| column.name == *name || column.name.eq_ignore_ascii_case(name)).map(|index| (index, name.clone()))).collect();
            self.columns = page.columns.clone();
            self.started = true;
        }
        self.rows = page_rows(&page.batch, first, &self.watched);
        Ok(())
    }

    /// Reads the first page, so the columns are known before the first row is asked for.
    pub(in crate::app) fn open(&mut self) -> Result<&[PageColumn], ReadError> {
        if !self.started {
            self.fetch()?;
        }
        Ok(&self.columns)
    }
}

impl<F: PageFeed> RowSource for QueryRows<'_, F> {
    fn next_row(&mut self) -> Result<Read, SourceFailure> {
        // A page can be empty and still have another after it only when the server says so; an empty last page ends it.
        while self.rows.is_empty() && self.cursor.more() {
            self.fetch().map_err(SourceFailure::from)?;
        }
        match self.rows.pop_front() {
            Some(Read::Row(row)) => {
                self.bytes.fetch_add(row.cells.iter().flatten().map(|cell| cell.len() as u64).sum(), Ordering::Relaxed);
                Ok(Read::Row(row))
            }
            Some(other) => Ok(other),
            None => Ok(Read::End),
        }
    }
}

/// What the user is told where a query is typed: its session is in UTC (see [`PageReader::open`]).
pub(in crate::app) const QUERY_UTC_NOTE: &str = "The query runs in UTC: NOW(), CURDATE() and timestamp literals in WHERE are read as UTC.";

/// Whether the session that reads a source of `kind` is set to UTC, so a query in it means UTC.
pub(in crate::app) fn pins_session_zone(kind: ConnectionKind) -> bool {
    matches!(kind, ConnectionKind::Mysql | ConnectionKind::Mariadb | ConnectionKind::Clickhouse)
}

/// A second session of the source's connection, reading pages.
pub(in crate::app) struct PageReader {
    session: DriverSession,
    handle: Arc<DriverHandle>,
    control: Arc<ApplyControl>,
    /// The driver can run a statement read-only (a query typed by the user cannot write).
    read_only: bool,
    kind: ConnectionKind,
    /// The database the statements run on.
    database: Option<String>,
    /// The snapshot of the table being read.
    snapshot: SnapshotState,
    /// The `ROLLBACK` that ended the last table's transaction failed: the transaction may still be open.
    stale: bool,
    /// The page whose data the last call returned: released with the next.
    held: Option<u64>,
    /// This session's own SSH tunnel, when the connection has one. Declared last: it goes after the session has
    /// disconnected through it.
    _tunnel: Option<engine_connect::connect::LiveTunnel>,
}

/// Why the session of a transfer could not be opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) struct OpenError(pub(in crate::app) String);

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl PageReader {
    /// Blocking: opens the session. `read_only`: the driver can run a statement read-only. The session works in UTC where a server
    /// converts a `timestamp` by the zone of the session (MySQL, MariaDB, ClickHouse), for a table and for a query alike: the
    /// writer stores what it is given as UTC, so a wall clock in the zone of the server would be shifted. A server that refuses the
    /// setting, or ignores it (ClickHouse before 23.6), fails the open. PostgreSQL is left alone: a `timestamptz` arrives with its
    /// offset.
    pub(in crate::app) fn open(spec: SessionSpec, control: Arc<ApplyControl>, read_only: bool, kind: ConnectionKind, database: Option<String>) -> Result<Self, OpenError> {
        let handle = Arc::clone(&spec.handle);
        let (session, tunnel) = spec.open().map_err(OpenError)?;
        let mut reader = Self { session, handle, control, read_only, kind, database, snapshot: SnapshotState::Unavailable("not asked for"), stale: false, held: None, _tunnel: tunnel };
        reader.pin_utc()?;
        Ok(reader)
    }

    fn pin_utc(&mut self) -> Result<(), OpenError> {
        if !pins_session_zone(self.kind) {
            return Ok(());
        }
        match self.kind {
            ConnectionKind::Mysql | ConnectionKind::Mariadb => self.statement("SET time_zone = '+00:00'").map_err(|error| OpenError(format!("The session could not be set to UTC to read timestamps: {error}"))),
            ConnectionKind::Clickhouse => {
                // Before 23.6 the setting is unknown or ignored: what the session says its zone is, is what counts (a
                // server already in UTC reads right whatever the SET did).
                let _ = self.statement("SET session_timezone = 'UTC'");
                let zone = self.first_cell("SELECT timezone()");
                if zone.as_deref() != Some("UTC") {
                    return Err(OpenError("The source server does not take a session time zone (ClickHouse needs 23.6 or later): its DateTime values would be read in the zone of the server and shifted.".to_owned()));
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn open_snapshot(&mut self) -> SnapshotState {
        // A transaction the last table left open would be joined by `BEGIN` (PostgreSQL only warns) or committed by `START
        // TRANSACTION` (MySQL): end it first, or say there is no snapshot. Never `Held` on a stale transaction.
        if self.stale {
            if self.statement("ROLLBACK").is_err() {
                return SnapshotState::Unavailable("the transaction of the previous table could not be ended");
            }
            self.stale = false;
        }
        let begun = |this: &mut Self, sql: &str| match this.statement(sql) {
            Ok(()) => SnapshotState::Held,
            Err(_) => SnapshotState::Unavailable("the server refused to open the transaction"),
        };
        match self.kind {
            ConnectionKind::Postgres => begun(self, "BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY"),
            // A consistent snapshot is taken at REPEATABLE READ only: under any other level the transaction would claim one it
            // does not have.
            ConnectionKind::Mysql | ConnectionKind::Mariadb => match self.statement("SET SESSION TRANSACTION ISOLATION LEVEL REPEATABLE READ") {
                Ok(()) => begun(self, "START TRANSACTION WITH CONSISTENT SNAPSHOT, READ ONLY"),
                Err(_) => SnapshotState::Unavailable("the server did not accept REPEATABLE READ, the isolation a snapshot needs"),
            },
            // A read transaction of a database that is not in WAL mode blocks the writers of that file, ours included.
            ConnectionKind::Sqlite => match self.first_cell("PRAGMA journal_mode") {
                Some(mode) if mode.eq_ignore_ascii_case("wal") => begun(self, "BEGIN"),
                _ => SnapshotState::Unavailable("the database is not in WAL mode: a read transaction would block its writers"),
            },
            ConnectionKind::Clickhouse => SnapshotState::Unavailable("ClickHouse has no snapshot across statements"),
            ConnectionKind::Mongodb | ConnectionKind::Redis => SnapshotState::Unavailable("not a SQL connection"),
        }
    }

    /// The text may only be one plain read: the statement the driver pages (see [`read_refusal`]). Lexed as the session's server
    /// reads it: its `sql_mode` (`NO_BACKSLASH_ESCAPES`, `ANSI_QUOTES`, MariaDB's `MSSQL` brackets) and, for MySQL and MariaDB, its
    /// version (a `/*!80000 … */` comment is code or comment by it).
    fn check_is_a_read(&self, sql: &str) -> Result<(), ReadError> {
        let options = super::order::session_lex_options(self.kind, self.session.connection_dialect(), self.session.server_version());
        read_refusal(sql, &options).map_or(Ok(()), |message| Err(ReadError::Refused(message)))
    }

    fn request(&mut self, sql: &str, limit: u64) -> Result<QueryRequest, ReadError> {
        let operation = self.session.begin_operation().map_err(|error| self.failure(&error))?;
        Ok(QueryRequest {
            kind: "sql".into(),
            operation_id: operation.id(),
            database: self.database.clone(),
            object: None,
            offset: 0,
            limit,
            payload: serde_json::json!({ "sql": sql, "confirmed": true }),
        })
    }

    /// Runs a statement that returns nothing the reader wants (`BEGIN`, `SET`).
    fn statement(&mut self, sql: &str) -> Result<(), ReadError> {
        let request = self.request(sql, 1)?;
        let json = serde_json::to_string(&request).map_err(|error| ReadError::Refused(error.to_string()))?;
        let page = self.session.open_query(&json).map_err(|error| self.failure(&error))?;
        let _ = self.session.release_page(page.page_id);
        Ok(())
    }

    /// The first cell of the first row of a statement, as text.
    fn first_cell(&mut self, sql: &str) -> Option<String> {
        super::super::data_import::writer::first_cell(&mut self.session, self.database.clone(), sql)
    }

    fn release_held(&mut self) {
        if let Some(page) = self.held.take() {
            let _ = self.session.release_page(page);
        }
    }

    fn failure(&self, error: &LoaderError) -> ReadError {
        if self.control.is_cancelled() {
            return ReadError::Cancelled;
        }
        let text = crate::sessions::redact_secrets(&error.to_string());
        match error {
            LoaderError::DriverRejected { code, .. } if code.ends_with(".cancelled") => ReadError::Cancelled,
            LoaderError::DriverRejected { code, .. } if crate::sessions::is_connection_lost_code(code) => ReadError::Lost(text),
            LoaderError::DriverRejected { code, message, .. } if code.starts_with("query.") => {
                if code == codes::QUERY_NOT_PAGEABLE {
                    ReadError::Refused(format!("Only a single SELECT can be transferred ({})", crate::sessions::redact_secrets(message)))
                } else {
                    ReadError::Refused(super::super::data_import::message_of(&text))
                }
            }
            _ => ReadError::Lost(text),
        }
    }
}

/// Why `sql` may not be transferred: it must be one plain read. Said before anything is sent, on every dialect: inside a transaction
/// of ours the driver cannot always run a statement read-only itself, and a script could end the transaction (`COMMIT`) and go on
/// to write, or a statement could commit by itself (`CREATE TABLE … AS SELECT`, `SELECT … INTO`). Classified with the lexer of the
/// session, as the driver does.
pub(in crate::app) fn read_refusal(sql: &str, options: &engine_driver_kit::sql::SqlLexOptions) -> Option<String> {
    let class = engine_driver_kit::sql::classify(sql, options);
    if class.single().is_none() {
        return Some("Only a single SELECT can be transferred: this text is a script of several statements.".to_owned());
    }
    if let Some(refusal) = class.read_only_refusal() {
        return Some(format!("Only a read can be transferred: {refusal}."));
    }
    if super::order::has_locking_clause(sql, options) {
        return Some("Only a plain SELECT can be transferred: a locking clause (FOR UPDATE, FOR SHARE, FOR NO KEY UPDATE, FOR KEY SHARE, LOCK IN SHARE MODE) would hold row locks on the source for the whole transfer.".to_owned());
    }
    (!class.pageable).then(|| "Only a plain SELECT can be transferred: this statement writes, creates something or is not a query.".to_owned())
}

impl PageFeed for PageReader {
    fn dialect(&self) -> Option<SqlTarget> {
        let info = self.session.connection_dialect()?;
        Some(SqlTarget { dialect: crate::sql_table::SqlDialect::from_capabilities(info), binary: crate::sql_table::BinaryLiteral::for_kind(self.kind) })
    }

    fn begin_table(&mut self) -> SnapshotState {
        self.end_table();
        self.snapshot = self.open_snapshot();
        self.snapshot.clone()
    }

    fn end_table(&mut self) {
        self.release_held();
        if self.snapshot == SnapshotState::Held {
            // After a failed page PostgreSQL's transaction is aborted: the ROLLBACK is what lets the next table begin.
            self.stale = self.statement("ROLLBACK").is_err();
        }
        self.snapshot = SnapshotState::Unavailable("no table is being read");
    }

    fn fetch(&mut self, query: &Query, offset: u64, limit: u64) -> Result<Page, ReadError> {
        self.release_held();
        if self.control.is_cancelled() {
            return Err(ReadError::Cancelled);
        }
        self.check_is_a_read(&query.sql)?;
        let operation = self.session.begin_operation().map_err(|error| self.failure(&error))?;
        let operation_id = operation.id();
        let (handle, connection_id) = (Arc::clone(&self.handle), self.session.connection_id());
        let canceller: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            let _ = handle.cancel_operation(connection_id, &CancelRequest { operation_id });
        });
        self.control.register(canceller).map_err(|_| ReadError::Cancelled)?;
        let mut payload = serde_json::json!({ "sql": query.sql });
        // PostgreSQL runs the statement in a savepoint of our transaction with `transaction_read_only` on, and a page that fails
        // is rolled back to the savepoint: the table's transaction is not poisoned. SQLite switches `query_only` on for it. MySQL
        // and MariaDB refuse a read-only run inside a transaction that is open: ours is `READ ONLY` itself, and the text was
        // checked above.
        let family_in_transaction = self.snapshot == SnapshotState::Held && matches!(self.kind, ConnectionKind::Mysql | ConnectionKind::Mariadb);
        if self.read_only && !family_in_transaction {
            payload["read_only"] = true.into();
        }
        let request = QueryRequest { kind: "sql".into(), operation_id, database: query.database.clone(), object: None, offset, limit, payload };
        let result = (|| {
            let json = serde_json::to_string(&request).map_err(|error| LoaderError::MalformedResponse(error.to_string()))?;
            let page = self.session.open_query(&json)?;
            self.held = Some(page.page_id);
            let lease = self.session.acquire_arrow_page(page.page_id, 0, page.row_count).map_err(|error| LoaderError::MalformedResponse(error.to_string()))?;
            // SAFETY: the lease holds the Arrow C Data pair the driver exported for this page; import consumes it once.
            let imported = unsafe { lease.into_record_batch() }.map_err(|error| LoaderError::MalformedResponse(error.to_string()))?;
            Ok::<_, LoaderError>((page, imported))
        })();
        self.control.clear_running();
        let (page, imported) = result.map_err(|error| self.failure(&error))?;
        // The buffers belong to the lease now: the driver's page can go.
        self.release_held();
        Ok(Page { batch: imported.batch.clone(), columns: page.columns, more: page.flags.truncated, _lease: Some(imported) })
    }
}

impl Drop for PageReader {
    fn drop(&mut self) {
        self.end_table();
        let _ = self.session.disconnect();
    }
}

#[cfg(test)]
pub(in crate::app) mod memory {
    //! A table in memory that pages like a driver does, for the tests of everything above the reader.

    use super::*;
    use arrow_array::{ArrayRef, Int64Array, StringArray};
    use arrow_schema::{DataType, Field, Schema};

    /// `rows` rows of `id` (bigint) and `name` (text), the name of every `null_every`th row NULL; `fail_at_offset`
    /// makes the call at that offset fail with `error`.
    pub(in crate::app) struct MemoryFeed {
        pub(in crate::app) rows: u64,
        pub(in crate::app) null_every: u64,
        pub(in crate::app) fail_at_offset: Option<(u64, ReadError)>,
        pub(in crate::app) calls: Vec<(u64, u64)>,
        /// The statement of each call.
        pub(in crate::app) sqls: Vec<String>,
        pub(in crate::app) cancel_at_call: Option<(usize, Arc<ApplyControl>)>,
        /// The dialect of the session that reads.
        pub(in crate::app) dialect: Option<SqlTarget>,
        /// `name` is the key and is text (`name 1`, `name 2`, …) instead of `id`.
        pub(in crate::app) text_key: bool,
        /// `name` arrives as bytes (Arrow Binary), as a column with text that is not valid UTF-8 does.
        pub(in crate::app) bytes_name: bool,
        /// `begin`, `fetch` and `end`, in the order they were asked for.
        pub(in crate::app) events: Vec<&'static str>,
        /// What `begin_table` answers.
        pub(in crate::app) snapshot: SnapshotState,
        /// What `notes` answers.
        pub(in crate::app) notes: Vec<String>,
        /// A statement containing this text fails; with `abort`, like PostgreSQL's transaction: until `end_table` every
        /// statement fails with "current transaction is aborted".
        pub(in crate::app) fail_on: Option<(String, ReadError)>,
        pub(in crate::app) abort: bool,
        aborted: bool,
    }

    impl MemoryFeed {
        pub(in crate::app) fn new(rows: u64) -> Self {
            Self { rows, null_every: 0, fail_at_offset: None, calls: Vec::new(), sqls: Vec::new(), cancel_at_call: None, dialect: None, text_key: false, bytes_name: false, events: Vec::new(), snapshot: SnapshotState::Held, notes: Vec::new(), fail_on: None, abort: false, aborted: false }
        }
    }

    pub(in crate::app) fn columns() -> Vec<PageColumn> {
        vec![
            PageColumn { id: "id".into(), name: "id".into(), data_type: "int8".into(), nullable: false },
            PageColumn { id: "name".into(), name: "name".into(), data_type: "text".into(), nullable: true },
        ]
    }

    impl PageFeed for MemoryFeed {
        fn dialect(&self) -> Option<SqlTarget> {
            self.dialect
        }

        fn begin_table(&mut self) -> SnapshotState {
            self.events.push("begin");
            self.snapshot.clone()
        }

        fn end_table(&mut self) {
            self.events.push("end");
            self.aborted = false;
        }

        fn notes(&self) -> Vec<String> {
            self.notes.clone()
        }

        fn fetch(&mut self, query: &Query, offset: u64, limit: u64) -> Result<Page, ReadError> {
            self.events.push("fetch");
            if self.aborted {
                return Err(ReadError::Refused("current transaction is aborted, commands ignored until end of transaction block".into()));
            }
            if let Some((text, error)) = &self.fail_on {
                if query.sql.contains(text.as_str()) {
                    self.aborted = self.abort;
                    return Err(error.clone());
                }
            }
            self.calls.push((offset, limit));
            self.sqls.push(query.sql.clone());
            // A statement that asks for the rows after a key (`"id" > 2000`) starts there; the offset is for the others.
            let key_marker = if self.text_key { "> 'name " } else { "\"id\" > " };
            let offset = match query.sql.split_once(key_marker) {
                Some((_, rest)) => {
                    assert_eq!(offset, 0, "a read by key does not skip rows by offset");
                    rest.chars().take_while(char::is_ascii_digit).collect::<String>().parse().unwrap_or(0)
                }
                None => offset,
            };
            if let Some((control_call, control)) = &self.cancel_at_call {
                if self.calls.len() == *control_call {
                    control.cancel();
                }
            }
            if let Some((at, error)) = &self.fail_at_offset {
                if *at == offset {
                    return Err(error.clone());
                }
            }
            let end = (offset + limit).min(self.rows);
            let ids: Vec<i64> = (offset..end).map(|id| id as i64 + 1).collect();
            let names: Vec<Option<String>> = (offset..end).map(|id| (self.null_every == 0 || (id + 1) % self.null_every != 0).then(|| format!("name {}", id + 1))).collect();
            let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false), Field::new("name", DataType::Utf8, true)]));
            let (schema, name_array): (_, ArrayRef) = if self.bytes_name {
                let bytes: Vec<Option<Vec<u8>>> = names.iter().map(|name| name.as_ref().map(|name| name.clone().into_bytes())).collect();
                let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false), Field::new("name", DataType::Binary, true)]));
                (schema, Arc::new(arrow_array::BinaryArray::from_iter(bytes.iter().map(|bytes| bytes.as_deref()))))
            } else {
                (schema, Arc::new(StringArray::from(names)))
            };
            let arrays: Vec<ArrayRef> = vec![Arc::new(Int64Array::from(ids)), name_array];
            let batch = RecordBatch::try_new(schema, arrays).expect("a batch");
            Ok(Page::new(batch, columns(), end < self.rows))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::memory::MemoryFeed;
    use super::*;

    fn query() -> Query {
        Query::new(None, "SELECT * FROM t".into())
    }

    fn drain(rows: &mut QueryRows<'_, MemoryFeed>) -> Vec<Row> {
        let mut out = Vec::new();
        while let Read::Row(row) = rows.next_row().unwrap() {
            out.push(row);
        }
        out
    }

    #[test]
    fn rows_come_page_after_page_numbered_from_one_with_nulls_as_none() {
        let mut feed = MemoryFeed::new(5_000);
        feed.null_every = 3;
        let bytes = Arc::new(AtomicU64::new(0));
        let mut rows = QueryRows::new(&mut feed, query(), bytes.clone());
        assert_eq!(rows.open().unwrap().len(), 2);
        let all = drain(&mut rows);
        assert_eq!(all.len(), 5_000);
        assert_eq!((all[0].number, all[4_999].number), (1, 5_000));
        assert_eq!(all[0].cells, [Some("1".to_owned()), Some("name 1".to_owned())]);
        assert_eq!(all[2].cells, [Some("3".to_owned()), None]);
        assert!(bytes.load(Ordering::Relaxed) > 5_000);
        // Offsets follow the rows read; the last page ends the read.
        let offsets: Vec<u64> = feed.calls.iter().map(|(offset, _)| *offset).collect();
        assert_eq!(offsets[0], 0);
        assert!(offsets.windows(2).all(|pair| pair[0] < pair[1]) && *offsets.last().unwrap() < 5_000, "{offsets:?}");
    }

    fn key(columns: &[(&str, &str)]) -> Vec<(String, String)> {
        columns.iter().map(|(name, data_type)| ((*name).to_owned(), (*data_type).to_owned())).collect()
    }

    fn postgres() -> SqlTarget {
        SqlTarget { dialect: crate::sql_table::SqlDialect::POSTGRES, binary: crate::sql_table::BinaryLiteral::Postgres }
    }

    fn mysql() -> SqlTarget {
        SqlTarget { dialect: crate::sql_table::SqlDialect::MYSQL, binary: crate::sql_table::BinaryLiteral::HexString }
    }

    fn keyset(kind: ConnectionKind, columns: &[(&str, &str)]) -> Option<Keyset> {
        let target = if kind == ConnectionKind::Postgres { postgres() } else { mysql() };
        Keyset::for_table("SELECT * FROM t", &key(columns), true, target, kind)
    }

    #[test]
    fn a_unique_key_of_comparable_columns_pages_by_key_and_anything_else_by_offset() {
        let select = "SELECT * FROM \"public\".\"t\"";
        let pg = |columns: &[(&str, &str)]| Keyset::for_table(select, &key(columns), true, postgres(), ConnectionKind::Postgres);
        let one = pg(&[("id", "bigint")]).expect("a keyset");
        assert_eq!(one.first(), "SELECT * FROM \"public\".\"t\" ORDER BY \"id\"");
        assert_eq!(one.after(&["41".to_owned()], postgres()).unwrap(), "SELECT * FROM \"public\".\"t\" WHERE \"id\" > 41 ORDER BY \"id\"");
        // Several columns compare as a row; text is a literal with its quotes doubled; a name is quoted whatever it holds.
        let composite = pg(&[("a\"b", "text"), ("when", "timestamp without time zone"), ("n", "numeric(10,2)")]).expect("a keyset");
        assert_eq!(
            composite.after(&["it's".to_owned(), "2024-02-29 13:14:15.5".to_owned(), "-0.05".to_owned()], postgres()).unwrap(),
            "SELECT * FROM \"public\".\"t\" WHERE (\"a\"\"b\", \"when\", \"n\") > ('it''s', '2024-02-29 13:14:15.5', -0.05) ORDER BY \"a\"\"b\", \"when\", \"n\""
        );
        // No key, a key that is not unique (ClickHouse): offset.
        assert!(Keyset::for_table(select, &[], true, postgres(), ConnectionKind::Postgres).is_none());
        assert!(Keyset::for_table(select, &key(&[("id", "bigint")]), false, postgres(), ConnectionKind::Postgres).is_none());
        let clickhouse = SqlTarget { dialect: crate::sql_table::SqlDialect::CLICKHOUSE, binary: crate::sql_table::BinaryLiteral::ClickHouse };
        assert!(Keyset::for_table(select, &key(&[("id", "Int64")]), true, clickhouse, ConnectionKind::Clickhouse).is_none());
        // A name the dialect cannot quote safely is not made into a statement.
        assert!(Keyset::for_table(select, &key(&[("back\\slash", "bigint")]), true, clickhouse, ConnectionKind::Postgres).is_none());
    }

    #[test]
    fn only_types_that_compare_the_way_they_order_are_keys() {
        use ConnectionKind::{Mysql, Postgres, Sqlite};
        let allowed = |kind, types: &[&str]| types.iter().map(|data_type| key_type_allowed(kind, data_type)).collect::<Vec<_>>();
        assert_eq!(allowed(Postgres, &["integer", "bigint", "numeric(12,2)", "text", "character varying(20)", "uuid", "date", "timestamp without time zone", "timestamp with time zone"]), [true; 9]);
        assert_eq!(allowed(Postgres, &["double precision", "real", "bytea", "boolean", "jsonb", "integer[]", "mood", "interval", "time without time zone", "citext", "money"]), [false; 11]);
        assert_eq!(allowed(Mysql, &["int", "bigint unsigned", "decimal(10,2)", "varchar(20)", "char(3)", "date", "datetime(6)", "timestamp"]), [true; 8]);
        // An enum is ordered by position and compared as text, a set and a bit are not text, a TEXT is a prefix, a year, a float.
        assert_eq!(allowed(Mysql, &["enum('a','b')", "set('a','b')", "bit(8)", "bit(1)", "text", "longtext", "blob", "varbinary(8)", "year", "double", "float", "json", "time"]), [false; 13]);
        assert_eq!(allowed(Sqlite, &["INTEGER", "INT", "bigint", "TEXT", "VARCHAR(20)", "unsigned big int", "NVARCHAR(5)"]), [true; 7]);
        // No declared type (every value is its own storage class), numeric affinity, dates, blobs, floats.
        assert_eq!(allowed(Sqlite, &["", "NUMERIC", "DECIMAL(10,2)", "DATE", "DATETIME", "BLOB", "REAL", "DOUBLE", "BOOLEAN"]), [false; 9]);
        assert!(!key_type_allowed(ConnectionKind::Clickhouse, "Int64"));
        assert!(keyset(ConnectionKind::Mysql, &[("e", "enum('a','b')")]).is_none());
    }

    #[test]
    fn mysql_expands_a_row_comparison_into_ranges_of_its_index() {
        let composite = keyset(ConnectionKind::Mysql, &[("b", "int"), ("a", "varchar(10)"), ("c", "bigint")]).expect("a keyset");
        assert_eq!(
            composite.after(&["1".to_owned(), "x'y".to_owned(), "9".to_owned()], mysql()).unwrap(),
            "SELECT * FROM t WHERE `b` > 1 OR (`b` = 1 AND `a` > 'x''y') OR (`b` = 1 AND `a` = 'x''y' AND `c` > 9) ORDER BY `b`, `a`, `c`"
        );
        let one = keyset(ConnectionKind::Mysql, &[("id", "int")]).unwrap();
        assert_eq!(one.after(&["5".to_owned()], mysql()).unwrap(), "SELECT * FROM t WHERE `id` > 5 ORDER BY `id`");
        // The other dialects compare rows.
        assert!(keyset(ConnectionKind::Postgres, &[("a", "int"), ("b", "int")]).unwrap().after(&["1".to_owned(), "2".to_owned()], postgres()).unwrap().contains("WHERE (\"a\", \"b\") > (1, 2)"));
    }

    #[test]
    fn a_keyset_read_asks_for_the_rows_after_the_last_key_never_for_an_offset() {
        let mut feed = MemoryFeed::new(5_000);
        let keyset = keyset(ConnectionKind::Postgres, &[("id", "bigint")]).unwrap();
        let mut rows = QueryRows::new(&mut feed, Query { database: None, sql: "SELECT * FROM t ORDER BY \"id\"".into(), keyset: Some(keyset), order: RowOrder::Key }, Arc::default());
        let all = drain(&mut rows);
        assert_eq!(all.len(), 5_000);
        assert_eq!(all.iter().map(|row| row.number).collect::<Vec<_>>(), (1..=5_000).collect::<Vec<_>>(), "no row twice, none missed");
        assert_eq!(feed.sqls[0], "SELECT * FROM t ORDER BY `id`".replace('`', "\""));
        assert_eq!(feed.sqls[1], format!("SELECT * FROM t WHERE \"id\" > {FIRST_PAGE_ROWS} ORDER BY \"id\""));
        assert!(feed.calls.iter().all(|(offset, _)| *offset == 0));
    }

    #[test]
    fn the_literals_of_a_keyset_read_follow_the_dialect_of_the_session_that_reads() {
        // The connection was saved as a PostgreSQL-like dialect; the session that reads is a MySQL that escapes backslashes.
        let mut feed = MemoryFeed::new(5_000);
        feed.dialect = Some(mysql());
        feed.text_key = true;
        let keyset = Keyset::for_table("SELECT * FROM t", &key(&[("name", "varchar(20)")]), true, postgres(), ConnectionKind::Mysql).unwrap();
        let mut rows = QueryRows::new(&mut feed, Query { database: None, sql: "SELECT * FROM t".into(), keyset: Some(keyset), order: RowOrder::Key }, Arc::default());
        let _ = rows.open().unwrap();
        while !matches!(rows.next_row().unwrap(), Read::End) {}
        assert!(feed.sqls[1].contains("> 'name 2000'"), "{}", feed.sqls[1]);
        // A backslash in a key is doubled for the dialect that reads it.
        let keyset = Keyset::for_table("SELECT * FROM t", &key(&[("name", "varchar(20)")]), true, postgres(), ConnectionKind::Mysql).unwrap();
        assert!(keyset.after(&["a\\b".to_owned()], mysql()).unwrap().contains("'a\\\\b'"));
        assert!(keyset.after(&["a\\b".to_owned()], postgres()).unwrap().contains("'a\\b'"));
    }

    #[test]
    fn a_null_key_ends_a_keyset_read_with_why() {
        let mut feed = MemoryFeed::new(100);
        feed.null_every = 1; // the name is NULL in every row: a key on it is
        let keyset = keyset(ConnectionKind::Postgres, &[("name", "text")]).unwrap();
        let mut rows = QueryRows::new(&mut feed, Query { database: None, sql: "SELECT * FROM t".into(), keyset: Some(keyset), order: RowOrder::Key }, Arc::default());
        assert!(matches!(rows.open(), Err(ReadError::Refused(message)) if message.contains("NULL")));
    }

    #[test]
    fn bytes_in_a_column_the_server_calls_text_make_a_bad_row_not_a_value() {
        let mut feed = MemoryFeed::new(5);
        feed.bytes_name = true;
        let mut rows = QueryRows::new(&mut feed, query(), Arc::default());
        rows.refuse_bytes_in(vec!["name".to_owned()]);
        let mut seen = Vec::new();
        loop {
            match rows.next_row().unwrap() {
                Read::End => break,
                other => seen.push(other),
            }
        }
        assert_eq!(seen.len(), 5);
        assert!(seen.iter().all(|read| matches!(read, Read::Bad(bad) if matches!(&bad.problem, RowProblem::BytesInText(column) if column == "name"))), "{seen:?}");
        // Not watched (the target holds bytes): the hex text is the value.
        let mut feed = MemoryFeed::new(2);
        feed.bytes_name = true;
        let mut rows = QueryRows::new(&mut feed, query(), Arc::default());
        assert!(matches!(rows.next_row().unwrap(), Read::Row(row) if row.cells[1].as_deref() == Some("0x6e616d652031")));
    }

    #[test]
    fn a_text_key_that_arrives_as_bytes_goes_on_by_offset_in_the_order_of_the_key() {
        let mut feed = MemoryFeed::new(5_000);
        feed.text_key = true;
        feed.bytes_name = true;
        let keyset = keyset(ConnectionKind::Postgres, &[("name", "text")]).unwrap();
        let mut rows = QueryRows::new(&mut feed, Query { database: None, sql: "SELECT * FROM t ORDER BY \"name\"".into(), keyset: Some(keyset), order: RowOrder::Key }, Arc::default());
        let ids: Vec<String> = drain(&mut rows).into_iter().map(|row| row.cells[0].clone().unwrap()).collect();
        assert_eq!(ids.len(), 5_000);
        assert_eq!(ids.iter().collect::<std::collections::HashSet<_>>().len(), 5_000, "every row once");
        // The first page asked for the key, the next ones never compared the bytes as a string: offsets, in the same order.
        assert!(feed.sqls.iter().all(|sql| !sql.contains(" WHERE ") && sql.contains("ORDER BY")), "{:?}", feed.sqls);
        assert_eq!(feed.calls.iter().map(|(offset, _)| *offset).take(2).collect::<Vec<_>>(), [0, 2_000]);
    }

    #[test]
    fn only_one_plain_read_may_be_transferred_whatever_the_dialect() {
        use engine_driver_kit::sql::SqlLexOptions;
        let pg = SqlLexOptions::postgres();
        let mysql = SqlLexOptions::mysql();
        let wrapped = "SELECT * FROM (\nSELECT a FROM t\n) AS transfer_page ORDER BY \"a\"";
        for (sql, options) in [(wrapped, &pg), ("SELECT * FROM t ORDER BY id", &mysql), ("WITH c AS (SELECT 1) SELECT * FROM c", &pg), ("SELECT 1 /* ; COMMIT */", &pg)] {
            assert_eq!(read_refusal(sql, options), None, "{sql}");
        }
        for (sql, options) in [
            ("SELECT 1; COMMIT; UPDATE t SET a = 1", &pg),
            ("SELECT 1; UPDATE t SET a = 1", &pg),
            ("SELECT 1; SELECT 2", &pg),
            ("COMMIT", &pg),
            ("UPDATE t SET a = 1", &pg),
            ("SELECT * INTO copy FROM t", &pg),
            ("WITH d AS (DELETE FROM t RETURNING *) SELECT * FROM d", &pg),
            ("CREATE TABLE copy AS SELECT * FROM t", &mysql),
            ("SELECT 1; COMMIT; UPDATE t SET a = 1", &mysql),
            ("SELECT * FROM t INTO OUTFILE '/tmp/x'", &mysql),
            ("SELECT 1 /*!50000 ; COMMIT */", &mysql),
            ("CALL p()", &mysql),
            ("", &pg),
            // A locking clause holds row locks on the source for the whole transfer, at the top and in the wrapper.
            ("SELECT * FROM t FOR UPDATE", &pg),
            ("SELECT * FROM t FOR SHARE", &pg),
            ("SELECT * FROM t FOR NO KEY UPDATE", &pg),
            ("SELECT * FROM t FOR KEY SHARE", &pg),
            ("SELECT * FROM t FOR UPDATE", &mysql),
            ("SELECT * FROM t LOCK IN SHARE MODE", &mysql),
            ("SELECT * FROM (\nSELECT a FROM t FOR UPDATE\n) AS transfer_page ORDER BY \"a\"", &pg),
        ] {
            assert!(read_refusal(sql, options).is_some(), "{sql}");
        }
        // The wrapped statement of a script is still a script: the semicolons split it inside the parentheses too.
        assert!(read_refusal("SELECT * FROM (\nSELECT 1; COMMIT; UPDATE t SET a = 1\n) AS transfer_page ORDER BY \"a\"", &pg).is_some());
    }

    #[test]
    fn a_locking_clause_is_refused_with_why() {
        let message = read_refusal("SELECT * FROM t FOR UPDATE", &engine_driver_kit::sql::SqlLexOptions::postgres()).unwrap();
        assert!(message.contains("locking clause") && message.contains("FOR UPDATE") && message.contains("LOCK IN SHARE MODE"), "{message}");
    }

    #[test]
    fn the_session_is_set_to_utc_where_a_server_converts_timestamps_by_its_zone_for_a_table_and_a_query_alike() {
        use ConnectionKind::*;
        // `PageReader::open` pins whenever this is true, whatever the source: tables and queries (live: `live_a_query_...`).
        assert_eq!([Mysql, Mariadb, Clickhouse, Postgres, Sqlite].map(pins_session_zone), [true, true, true, false, false]);
        assert!(QUERY_UTC_NOTE.starts_with("The query runs in UTC: NOW(), CURDATE() and timestamp literals in WHERE are read as UTC"));
    }

    #[test]
    fn an_empty_table_has_its_columns_and_no_rows() {
        let mut feed = MemoryFeed::new(0);
        let mut rows = QueryRows::new(&mut feed, query(), Arc::default());
        assert_eq!(rows.open().unwrap().len(), 2);
        assert!(matches!(rows.next_row().unwrap(), Read::End));
        assert!(matches!(rows.next_row().unwrap(), Read::End), "asking again is still the end");
    }

    #[test]
    fn a_page_that_fails_ends_the_read_with_why() {
        let mut feed = MemoryFeed::new(10_000);
        feed.fail_at_offset = Some((FIRST_PAGE_ROWS, ReadError::Lost("connection reset".into())));
        let mut rows = QueryRows::new(&mut feed, query(), Arc::default());
        let mut read = 0;
        let failure = loop {
            match rows.next_row() {
                Ok(Read::Row(_)) => read += 1,
                Ok(_) => panic!("ended early"),
                Err(failure) => break failure,
            }
        };
        assert_eq!((read, failure), (FIRST_PAGE_ROWS, SourceFailure::Failed("connection reset".into())));
        let mut cancelled = MemoryFeed::new(10);
        cancelled.fail_at_offset = Some((0, ReadError::Cancelled));
        let mut rows = QueryRows::new(&mut cancelled, query(), Arc::default());
        assert_eq!(rows.open().err(), Some(ReadError::Cancelled));
    }

    #[test]
    fn the_page_size_follows_the_width_of_the_rows() {
        // Narrow rows grow the page up to the cap, wide ones shrink it to what fits the target.
        assert_eq!(next_limit(2_000, 2_000, 2_000 * 40), MAX_PAGE_ROWS);
        assert_eq!(next_limit(2_000, 2_000, 2_000 * 100_000), (PAGE_TARGET_BYTES / 100_000) as u64);
        assert_eq!(next_limit(2_000, 100, 100 * 50_000_000), MIN_PAGE_ROWS, "a page is never less than a few rows");
        assert_eq!(next_limit(777, 0, 0), 777, "an empty page changes nothing");
    }
}
