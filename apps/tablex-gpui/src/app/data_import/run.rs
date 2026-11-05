//! The load: rows of a [`RowSource`] (a file, or the pages of a query) into batches, batches into a
//! [`BatchWriter`], with the error policy, the transaction mode, progress and cancel. Nothing here knows about a
//! driver or a window.
//!
//! Memory is one batch of converted rows (plus the row being read): `batch_size` rows (or [`BATCH_BYTES_CAP`] bytes
//! of cells, whichever comes first), however large the source.
//!
//! Errors, by who is wrong:
//! * a **row** that cannot be read or converted (`RowError`): with *stop* the load ends there, with *skip* it is
//!   logged (the first [`ERROR_LOG_CAP`] only; all are counted) and the load goes on;
//! * a **batch** the server refuses (`WriteFailure::Rejected`): with *stop* the load ends; with *skip* the
//!   batch is sent again one row at a time, so only the rows the server refuses are lost;
//! * a batch that was **partly applied** (`WriteFailure::Partial`: a table without transactions took some of its rows
//!   and refused one) is never sent again, whatever the policy: the rows it took are counted, and the load ends;
//! * **anything else** (the connection is gone, the user cancelled) always ends the load.
//!
//! Transactions: *commit per batch* leaves every batch that went in; *one transaction* commits at the very end
//! and rolls everything back when the load stops, fails or is cancelled. A COMMIT that did not answer may have been
//! applied: those rows are reported as *uncertain*, never as rolled back.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

use super::convert::Converter;
use crate::import::infer::Hints;
use crate::import::source::{ImportError, Read, SourceReader};

/// Where the rows come from: the file of an import or the pages of a query of a data transfer. One call is one row.
pub(in crate::app) trait RowSource {
    fn next_row(&mut self) -> Result<Read, SourceFailure>;
}

/// Why a source cannot go on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) enum SourceFailure {
    /// The user cancelled while the source waited for its data.
    Cancelled,
    /// The file is unreadable, the connection is gone, the query was refused.
    Failed(String),
}

impl RowSource for SourceReader {
    fn next_row(&mut self) -> Result<Read, SourceFailure> {
        SourceReader::next_row(self).map_err(|error: ImportError| SourceFailure::Failed(error.to_string()))
    }
}

/// A batch is sent once the text of its cells reaches this many bytes, however few rows it holds: rows of large
/// values (documents, images) must not pile up to `batch_size` of them.
pub(in crate::app) const BATCH_BYTES_CAP: usize = 16 * 1024 * 1024;

/// The most skipped rows the report lists. More are counted, not kept.
pub(in crate::app) const ERROR_LOG_CAP: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum TransactionMode {
    /// Every batch commits by itself.
    PerBatch,
    /// One transaction for the whole load.
    Whole,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum OnError {
    Stop,
    SkipAndLog,
}

/// How the load behaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) struct LoadSettings {
    pub(in crate::app) batch_size: usize,
    pub(in crate::app) transaction: TransactionMode,
    pub(in crate::app) on_error: OnError,
}

impl LoadSettings {
    pub(in crate::app) const DEFAULT_BATCH: usize = 1000;
    pub(in crate::app) const MAX_BATCH: usize = 50_000;
}

/// What the writer reports when a call does not succeed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) enum WriteFailure {
    /// The user cancelled the statement that was running.
    Cancelled,
    /// The server refused this data. The session is fine.
    Rejected(String),
    /// A table without transactions took `applied` rows of the batch and then refused one; `unknown` rows were cut
    /// off and may or may not have been written. Nothing of it was undone.
    Partial { applied: u64, unknown: u64, message: String },
    /// The session cannot go on (the connection is gone, the driver refused the request).
    Lost(String),
}

impl std::fmt::Display for WriteFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => f.write_str("cancelled"),
            Self::Rejected(message) | Self::Lost(message) | Self::Partial { message, .. } => f.write_str(message),
        }
    }
}

/// Where the rows go. One call is one atomic step on the server.
pub(in crate::app) trait BatchWriter {
    /// How many bytes of cell text a batch may hold before it is sent, however few rows it has.
    fn batch_bytes_cap(&self) -> usize {
        BATCH_BYTES_CAP
    }

    /// Opens the transaction of a whole-load import.
    fn begin(&mut self) -> Result<(), WriteFailure>;
    /// Inserts `rows` (column name to value) in one step: all of them or none. Gives the rows back (the writer
    /// moves them into its request and takes them out again, so none is copied) with the rows inserted.
    fn insert(&mut self, rows: Vec<Map<String, Value>>) -> (Vec<Map<String, Value>>, Result<u64, WriteFailure>);
    fn commit(&mut self) -> Result<(), WriteFailure>;
    /// Ends the transaction without keeping anything. Failures are the connection's to report.
    fn rollback(&mut self);
}

/// A column of the table that a JSON key outside the preview goes to, by name (case does not matter).
#[derive(Debug, Clone)]
pub(in crate::app) struct ExtraColumn {
    pub(in crate::app) key: String,
    pub(in crate::app) target: String,
    pub(in crate::app) converter: Converter,
}

/// One source column going to one target column.
#[derive(Debug, Clone)]
pub(in crate::app) struct ColumnPlan {
    pub(in crate::app) source: usize,
    pub(in crate::app) target: String,
    pub(in crate::app) converter: Converter,
}

/// Why a row was left out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) struct RowError {
    /// The row among the data rows of the file (1-based).
    pub(in crate::app) row: u64,
    /// Where the row starts in its file; `0` for a row that did not come from a file.
    pub(in crate::app) line: u64,
    pub(in crate::app) message: String,
}

impl std::fmt::Display for RowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.line == 0 {
            write!(f, "Row {}: {}", self.row, self.message)
        } else {
            write!(f, "Row {} (line {}): {}", self.row, self.line, self.message)
        }
    }
}

/// How a load ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) enum Outcome {
    Completed,
    /// A row or batch was refused and the policy is to stop.
    Stopped(String),
    Cancelled,
    /// The file or the connection failed.
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) struct Report {
    pub(in crate::app) outcome: Outcome,
    /// Rows now in the table.
    pub(in crate::app) inserted: u64,
    /// Rows the load took out again with the rollback of its one transaction.
    pub(in crate::app) rolled_back: u64,
    /// Rows left out (and counted in `errors_total`).
    pub(in crate::app) skipped: u64,
    pub(in crate::app) rows_read: u64,
    /// The first [`ERROR_LOG_CAP`] skipped rows.
    pub(in crate::app) errors: Vec<RowError>,
    pub(in crate::app) errors_total: u64,
    /// Rows whose outcome is not known: a COMMIT that did not answer, or a statement that was cut off.
    pub(in crate::app) uncertain: u64,
    pub(in crate::app) bytes: u64,
    pub(in crate::app) elapsed: Duration,
}

impl Report {
    pub(in crate::app) fn rows_per_second(&self) -> f64 {
        let seconds = self.elapsed.as_secs_f64();
        if seconds > 0.0 {
            (self.inserted + self.rolled_back) as f64 / seconds
        } else {
            0.0
        }
    }
}

/// Counters the UI reads while a load runs.
#[derive(Debug, Default)]
pub(in crate::app) struct Progress {
    pub(in crate::app) rows_read: AtomicU64,
    pub(in crate::app) inserted: AtomicU64,
    pub(in crate::app) skipped: AtomicU64,
    /// The file's bytes read so far, shared with the reader.
    pub(in crate::app) bytes: Arc<AtomicU64>,
}

impl Progress {
    pub(in crate::app) fn snapshot(&self) -> ProgressSnapshot {
        ProgressSnapshot {
            rows_read: self.rows_read.load(Ordering::Relaxed),
            inserted: self.inserted.load(Ordering::Relaxed),
            skipped: self.skipped.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(in crate::app) struct ProgressSnapshot {
    pub(in crate::app) rows_read: u64,
    pub(in crate::app) inserted: u64,
    pub(in crate::app) skipped: u64,
    pub(in crate::app) bytes: u64,
}

/// What a load needs besides the writer.
pub(in crate::app) struct Load<'a> {
    pub(in crate::app) columns: &'a [ColumnPlan],
    /// Columns that JSON keys first seen after the preview go to.
    pub(in crate::app) extras: &'a [ExtraColumn],
    pub(in crate::app) hints: &'a Hints,
    pub(in crate::app) settings: LoadSettings,
    pub(in crate::app) progress: &'a Progress,
    pub(in crate::app) cancelled: &'a (dyn Fn() -> bool + Sync),
    /// Called at most every [`NOTIFY_EVERY`] with the counters, and once at the end.
    pub(in crate::app) notify: &'a (dyn Fn() + Sync),
}

/// How often the load tells the window it moved.
pub(in crate::app) const NOTIFY_EVERY: Duration = Duration::from_millis(120);

/// After this many batches in a row that the server refused row by row to the last, the load stops: every row
/// is refused for the same reason (a column that is not in the table), and trying them one at a time would be
/// a round trip per row of the file.
const REFUSED_BATCHES_LIMIT: u32 = 3;

struct Runner<'a, W> {
    load: &'a Load<'a>,
    writer: &'a mut W,
    /// The converted rows of the batch being filled, and where each came from (row, line) for the error log.
    rows: Vec<Map<String, Value>>,
    origins: Vec<(u64, u64)>,
    /// Bytes of text in the cells of `rows`.
    batch_bytes: usize,
    refused_batches: u32,
    errors: Vec<RowError>,
    errors_total: u64,
    inserted: u64,
    uncertain: u64,
    started: Instant,
    last_notify: Instant,
}

/// Why the loop ended early.
enum Halt {
    Stopped(String),
    Cancelled,
    Failed(String),
    /// A partly applied batch: what it wrote stays, and it is not tried again.
    Partial(String),
}

impl From<WriteFailure> for Halt {
    fn from(failure: WriteFailure) -> Self {
        match failure {
            WriteFailure::Cancelled => Self::Cancelled,
            WriteFailure::Rejected(message) | WriteFailure::Lost(message) => Self::Failed(message),
            WriteFailure::Partial { message, .. } => Self::Partial(message),
        }
    }
}

impl<W: BatchWriter> Runner<'_, W> {
    fn skip(&mut self, row: RowError) -> Result<(), Halt> {
        if self.load.settings.on_error == OnError::Stop {
            return Err(Halt::Stopped(row.to_string()));
        }
        self.errors_total += 1;
        self.load.progress.skipped.fetch_add(1, Ordering::Relaxed);
        if self.errors.len() < ERROR_LOG_CAP {
            self.errors.push(row);
        }
        Ok(())
    }

    fn convert(&self, row: &crate::import::source::Row) -> Result<Map<String, Value>, RowError> {
        let mut values = Map::with_capacity(self.load.columns.len());
        for plan in self.load.columns {
            let cell = row.cells.get(plan.source).and_then(|cell| cell.as_deref());
            let value = match cell {
                None => Value::Null,
                Some(text) if text.trim().is_empty() && plan.converter.empty_is_null() => Value::Null,
                Some(text) => plan.converter.convert(text, self.load.hints).map_err(|problem| RowError {
                    row: row.number,
                    line: row.line,
                    message: format!("\"{}\" {problem}: {}", plan.target, shown(text)),
                })?,
            };
            values.insert(plan.target.clone(), value);
        }
        for (key, cell) in &row.extra {
            let Some(extra) = self.load.extras.iter().find(|extra| extra.key.eq_ignore_ascii_case(key)) else {
                return Err(RowError {
                    row: row.number,
                    line: row.line,
                    message: format!("has the key \"{key}\", which is no column of the preview or of the table"),
                });
            };
            let value = match cell.as_deref() {
                None => Value::Null,
                Some(text) if text.trim().is_empty() && extra.converter.empty_is_null() => Value::Null,
                Some(text) => extra.converter.convert(text, self.load.hints).map_err(|problem| RowError {
                    row: row.number,
                    line: row.line,
                    message: format!("\"{}\" {problem}: {}", extra.target, shown(text)),
                })?,
            };
            values.insert(extra.target.clone(), value);
        }
        Ok(values)
    }

    fn notify(&mut self, force: bool) {
        if force || self.last_notify.elapsed() >= NOTIFY_EVERY {
            self.last_notify = Instant::now();
            (self.load.notify)();
        }
    }

    /// Sends the pending rows. A refused batch is sent again row by row when the policy is to skip.
    fn flush(&mut self) -> Result<(), Halt> {
        if self.rows.is_empty() {
            return Ok(());
        }
        let origins = std::mem::take(&mut self.origins);
        self.batch_bytes = 0;
        let (rows, result) = self.writer.insert(std::mem::take(&mut self.rows));
        match result {
            Ok(count) => {
                self.refused_batches = 0;
                self.landed(count);
            }
            Err(WriteFailure::Partial { applied, unknown, message }) => {
                // Never sent again: what it wrote is in the table.
                self.landed(applied);
                self.uncertain += unknown;
                return Err(Halt::Partial(message));
            }
            Err(WriteFailure::Rejected(message)) => {
                if self.load.settings.on_error == OnError::Stop {
                    let (first, last) = (origins[0].0, origins[origins.len() - 1].0);
                    return Err(Halt::Stopped(format!("The server refused rows {first}–{last}: {message}")));
                }
                if let [(row, line)] = origins[..] {
                    self.skip(RowError { row, line, message })?;
                    return Ok(());
                }
                self.retry_one_by_one(rows, &origins, message)?;
            }
            Err(other) => return Err(other.into()),
        }
        self.notify(false);
        Ok(())
    }

    /// The server refused a batch: every row goes alone, so only the rows it refuses are lost.
    fn retry_one_by_one(&mut self, rows: Vec<Map<String, Value>>, origins: &[(u64, u64)], message: String) -> Result<(), Halt> {
        let before = self.inserted;
        for (row, (number, line)) in rows.into_iter().zip(origins.iter().copied()) {
            if (self.load.cancelled)() {
                return Err(Halt::Cancelled);
            }
            match self.writer.insert(vec![row]).1 {
                Ok(count) => self.landed(count),
                Err(WriteFailure::Rejected(message)) => self.skip(RowError { row: number, line, message })?,
                Err(WriteFailure::Partial { applied, unknown, message }) => {
                    self.landed(applied);
                    self.uncertain += unknown;
                    return Err(Halt::Partial(message));
                }
                Err(other) => return Err(other.into()),
            }
        }
        self.refused_batches = if self.inserted == before { self.refused_batches + 1 } else { 0 };
        if self.refused_batches >= REFUSED_BATCHES_LIMIT {
            return Err(Halt::Stopped(format!("The server refused every row of the last {REFUSED_BATCHES_LIMIT} batches: {message}")));
        }
        Ok(())
    }

    fn landed(&mut self, count: u64) {
        self.inserted += count;
        self.load.progress.inserted.fetch_add(count, Ordering::Relaxed);
    }

    fn pump<S: RowSource>(&mut self, source: &mut S) -> Result<(), Halt> {
        loop {
            if (self.load.cancelled)() {
                return Err(Halt::Cancelled);
            }
            let read = source.next_row().map_err(|failure| match failure {
                SourceFailure::Cancelled => Halt::Cancelled,
                SourceFailure::Failed(message) => Halt::Failed(message),
            })?;
            match read {
                Read::End => return self.flush(),
                Read::Bad(bad) => {
                    self.load.progress.rows_read.fetch_add(1, Ordering::Relaxed);
                    self.skip(RowError { row: bad.number, line: bad.line, message: bad.problem.to_string() })?;
                }
                Read::Row(row) => {
                    self.load.progress.rows_read.fetch_add(1, Ordering::Relaxed);
                    match self.convert(&row) {
                        Ok(values) => {
                            self.rows.push(values);
                            self.origins.push((row.number, row.line));
                            self.batch_bytes += row.cells.iter().flatten().map(String::len).sum::<usize>();
                            if self.rows.len() >= self.load.settings.batch_size || self.batch_bytes >= self.writer.batch_bytes_cap() {
                                self.flush()?;
                            }
                        }
                        Err(error) => self.skip(error)?,
                    }
                }
            }
            self.notify(false);
        }
    }
}

/// `text` as the error log shows it: cut short, line breaks visible.
fn shown(text: &str) -> String {
    let mut shown: String = text.chars().take(40).collect();
    if text.chars().count() > 40 {
        shown.push('…');
    }
    format!("\"{}\"", shown.replace('\n', "\\n").replace('\r', "\\r"))
}

/// Runs the load to its end. Blocking: run it off the UI thread.
pub(in crate::app) fn run<W: BatchWriter, S: RowSource>(load: &Load<'_>, source: &mut S, writer: &mut W) -> Report {
    let started = Instant::now();
    let mut runner = Runner {
        load,
        writer,
        rows: Vec::with_capacity(load.settings.batch_size),
        origins: Vec::with_capacity(load.settings.batch_size),
        batch_bytes: 0,
        refused_batches: 0,
        errors: Vec::new(),
        errors_total: 0,
        inserted: 0,
        uncertain: 0,
        started,
        last_notify: started,
    };
    let whole = load.settings.transaction == TransactionMode::Whole;
    let halt = (|| {
        if whole {
            runner.writer.begin().map_err(Halt::from)?;
        }
        runner.pump(source)
    })();
    let mut rolled_back = 0;
    let outcome = match halt {
        Ok(()) if whole => match runner.writer.commit() {
            Ok(()) => Outcome::Completed,
            // The server answered that it did not commit: the transaction is gone.
            Err(WriteFailure::Rejected(message)) => {
                rolled_back = std::mem::take(&mut runner.inserted);
                runner.writer.rollback();
                Outcome::Failed(format!("The transaction was not committed and was rolled back: {message}"))
            }
            // No answer (the connection broke, the COMMIT was cut off): it may have been applied.
            Err(failure) => {
                runner.uncertain += std::mem::take(&mut runner.inserted);
                runner.writer.rollback();
                Outcome::Failed(format!(
                    "The COMMIT did not answer ({failure}): the {} rows of the transaction may or may not have been saved. Check the table before importing again.",
                    runner.uncertain
                ))
            }
        },
        Ok(()) => Outcome::Completed,
        Err(halt) => {
            // A partly applied batch was written to a table that cannot roll back; everything else is undone.
            if whole && !matches!(halt, Halt::Partial(_)) {
                rolled_back = std::mem::take(&mut runner.inserted);
            }
            if whole {
                runner.writer.rollback();
            }
            match halt {
                Halt::Stopped(reason) => Outcome::Stopped(reason),
                Halt::Cancelled => Outcome::Cancelled,
                Halt::Failed(reason) => Outcome::Failed(reason),
                Halt::Partial(message) => Outcome::Failed(format!("A batch was only partly applied and the table cannot undo it: {message}")),
            }
        }
    };
    runner.notify(true);
    Report {
        outcome,
        inserted: runner.inserted,
        rolled_back,
        skipped: runner.errors_total,
        rows_read: load.progress.rows_read.load(Ordering::Relaxed),
        errors: runner.errors,
        errors_total: runner.errors_total,
        uncertain: runner.uncertain,
        bytes: load.progress.bytes.load(Ordering::Relaxed),
        elapsed: runner.started.elapsed(),
    }
}

#[cfg(test)]
pub(in crate::app) mod tests {
    use super::*;
    use crate::import::delimited::DelimitedFormat;
    use crate::import::source::SourceOptions;
    use std::sync::atomic::AtomicBool;

    /// A table in memory that refuses the rows a test tells it to, and logs what the load asked of it.
    #[derive(Default)]
    pub(in crate::app) struct FakeWriter {
        pub(in crate::app) committed: Vec<Map<String, Value>>,
        /// Rows of the open transaction.
        open: Option<Vec<Map<String, Value>>>,
        pub(in crate::app) calls: Vec<String>,
        /// A row whose `id` is one of these is refused by the server.
        pub(in crate::app) refuse_ids: Vec<i64>,
        pub(in crate::app) lose_after_inserts: Option<usize>,
        /// What COMMIT answers instead of success.
        pub(in crate::app) commit_fails: Option<WriteFailure>,
        /// The insert call (1-based) that is partly applied: it writes `applied` rows, `unknown` are cut off.
        pub(in crate::app) partial_at: Option<(usize, u64, u64)>,
        inserts: usize,
    }

    impl FakeWriter {
        /// A table that refuses the rows whose `id` is one of `ids`.
        pub(in crate::app) fn refusing(ids: Vec<i64>) -> Self {
            Self { refuse_ids: ids, ..Default::default() }
        }

        fn write(&mut self, rows: &[Map<String, Value>]) -> Result<u64, WriteFailure> {
            if self.lose_after_inserts.is_some_and(|limit| self.inserts > limit) {
                return Err(WriteFailure::Lost("connection lost".into()));
            }
            if let Some((at, applied, unknown)) = self.partial_at.filter(|(at, ..)| *at == self.inserts) {
                let _ = at;
                self.committed.extend(rows.iter().take(applied as usize).cloned());
                return Err(WriteFailure::Partial { applied, unknown, message: "uses the MyISAM engine, which has no transactions".into() });
            }
            if let Some(row) = rows.iter().find(|row| row["id"].as_i64().is_some_and(|id| self.refuse_ids.contains(&id))) {
                return Err(WriteFailure::Rejected(format!("duplicate key value {}", row["id"])));
            }
            match &mut self.open {
                Some(open) => open.extend(rows.iter().cloned()),
                None => self.committed.extend(rows.iter().cloned()),
            }
            Ok(rows.len() as u64)
        }

        pub(in crate::app) fn ids(&self) -> Vec<i64> {
            self.committed.iter().filter_map(|row| row["id"].as_i64()).collect()
        }
    }

    impl BatchWriter for FakeWriter {
        fn begin(&mut self) -> Result<(), WriteFailure> {
            self.calls.push("begin".into());
            self.open = Some(Vec::new());
            Ok(())
        }

        fn insert(&mut self, rows: Vec<Map<String, Value>>) -> (Vec<Map<String, Value>>, Result<u64, WriteFailure>) {
            self.calls.push(format!("insert {}", rows.len()));
            self.inserts += 1;
            let result = self.write(&rows);
            (rows, result)
        }

        fn commit(&mut self) -> Result<(), WriteFailure> {
            self.calls.push("commit".into());
            if let Some(failure) = self.commit_fails.clone() {
                return Err(failure);
            }
            self.committed.extend(self.open.take().unwrap_or_default());
            Ok(())
        }

        fn rollback(&mut self) {
            self.calls.push("rollback".into());
            self.open = None;
        }
    }

    fn csv_file(rows: usize, bad_every: usize) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.csv");
        let mut text = String::from("id,name,score\n");
        for id in 1..=rows {
            let score = if bad_every > 0 && id % bad_every == 0 { "oops".to_owned() } else { format!("{id}.5") };
            text.push_str(&format!("{id},name {id},{score}\n"));
        }
        std::fs::write(&path, text).unwrap();
        (dir, path)
    }

    fn plan() -> Vec<ColumnPlan> {
        ["id:bigint", "name:text", "score:numeric"]
            .iter()
            .enumerate()
            .map(|(source, spec)| {
                let (target, data_type) = spec.split_once(':').unwrap();
                ColumnPlan { source, target: target.to_owned(), converter: Converter::new(data_type) }
            })
            .collect()
    }

    pub(in crate::app) fn load_csv(
        path: &std::path::Path,
        settings: LoadSettings,
        writer: &mut FakeWriter,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Report {
        let progress = Progress::default();
        let mut source = SourceReader::open(path, &SourceOptions::delimited(DelimitedFormat::default()), &[], progress.bytes.clone()).unwrap();
        let columns = plan();
        let hints = Hints::default();
        let load = Load { columns: &columns, extras: &[], hints: &hints, settings, progress: &progress, cancelled, notify: &|| {} };
        run(&load, &mut source, writer)
    }

    const PER_BATCH_STOP: LoadSettings = LoadSettings { batch_size: 4, transaction: TransactionMode::PerBatch, on_error: OnError::Stop };
    const NEVER: &(dyn Fn() -> bool + Sync) = &|| false;

    #[test]
    fn every_row_lands_in_batches_of_the_chosen_size() {
        let (_dir, path) = csv_file(10, 0);
        let mut writer = FakeWriter::default();
        let report = load_csv(&path, PER_BATCH_STOP, &mut writer, NEVER);
        assert_eq!(report.outcome, Outcome::Completed);
        assert_eq!((report.inserted, report.skipped, report.rows_read), (10, 0, 10));
        assert_eq!(writer.calls, ["insert 4", "insert 4", "insert 2"]);
        assert_eq!(writer.ids(), (1..=10).collect::<Vec<_>>());
        // Values are typed by the target column.
        assert_eq!(writer.committed[0]["id"], serde_json::json!(1));
        assert_eq!(writer.committed[0]["score"], serde_json::json!("1.5"));
        assert!(report.bytes > 0);
    }

    #[test]
    fn stop_ends_at_the_first_bad_row_and_keeps_the_batches_before_it() {
        let (_dir, path) = csv_file(10, 6);
        let mut writer = FakeWriter::default();
        let report = load_csv(&path, PER_BATCH_STOP, &mut writer, NEVER);
        let Outcome::Stopped(reason) = &report.outcome else { panic!("stopped, got {:?}", report.outcome) };
        assert!(reason.contains("Row 6 (line 7)") && reason.contains("\"score\" is not a number") && reason.contains("\"oops\""), "{reason}");
        // Rows 1-4 went in; rows 5 was waiting in the next batch and is not sent.
        assert_eq!(writer.ids(), [1, 2, 3, 4]);
        assert_eq!(report.inserted, 4);
    }

    #[test]
    fn skip_logs_the_bad_rows_and_loads_the_rest() {
        let (_dir, path) = csv_file(12, 5);
        let mut writer = FakeWriter::default();
        let settings = LoadSettings { on_error: OnError::SkipAndLog, ..PER_BATCH_STOP };
        let report = load_csv(&path, settings, &mut writer, NEVER);
        assert_eq!(report.outcome, Outcome::Completed);
        assert_eq!((report.inserted, report.skipped, report.rows_read), (10, 2, 12));
        assert_eq!(report.errors.iter().map(|error| error.row).collect::<Vec<_>>(), [5, 10]);
        assert_eq!(report.errors[0].line, 6);
        assert_eq!(writer.ids(), [1, 2, 3, 4, 6, 7, 8, 9, 11, 12]);
    }

    #[test]
    fn a_batch_the_server_refuses_is_sent_row_by_row_so_only_the_bad_rows_are_lost() {
        let (_dir, path) = csv_file(8, 0);
        let mut writer = FakeWriter { refuse_ids: vec![3, 8], ..Default::default() };
        let settings = LoadSettings { on_error: OnError::SkipAndLog, ..PER_BATCH_STOP };
        let report = load_csv(&path, settings, &mut writer, NEVER);
        assert_eq!(report.outcome, Outcome::Completed);
        assert_eq!(writer.ids(), [1, 2, 4, 5, 6, 7]);
        assert_eq!((report.inserted, report.skipped), (6, 2));
        assert!(report.errors[0].message.contains("duplicate key value 3"), "{:?}", report.errors);
        // Stop mode reports the rows of the refused batch.
        let mut strict = FakeWriter { refuse_ids: vec![3], ..Default::default() };
        let report = load_csv(&path, PER_BATCH_STOP, &mut strict, NEVER);
        let Outcome::Stopped(reason) = &report.outcome else { panic!("stopped") };
        assert!(reason.contains("rows 1–4") && reason.contains("duplicate key value 3"), "{reason}");
        assert!(strict.committed.is_empty());
    }

    #[test]
    fn one_transaction_commits_at_the_end_and_rolls_back_when_the_load_stops() {
        let (_dir, path) = csv_file(10, 0);
        let whole = LoadSettings { transaction: TransactionMode::Whole, ..PER_BATCH_STOP };
        let mut writer = FakeWriter::default();
        let report = load_csv(&path, whole, &mut writer, NEVER);
        assert_eq!(report.outcome, Outcome::Completed);
        assert_eq!(writer.calls, ["begin", "insert 4", "insert 4", "insert 2", "commit"]);
        assert_eq!(writer.committed.len(), 10);

        let (_bad_dir, bad_path) = csv_file(10, 6);
        let mut stopped = FakeWriter::default();
        let report = load_csv(&bad_path, whole, &mut stopped, NEVER);
        assert!(matches!(report.outcome, Outcome::Stopped(_)));
        assert_eq!((report.inserted, report.rolled_back), (0, 4));
        assert!(stopped.committed.is_empty(), "nothing is kept");
        assert_eq!(stopped.calls.last().map(String::as_str), Some("rollback"));

        // The server said it did not commit: rolled back, and the report says so.
        let mut refused = FakeWriter { commit_fails: Some(WriteFailure::Rejected("deferred constraint".into())), ..Default::default() };
        let report = load_csv(&path, whole, &mut refused, NEVER);
        assert!(matches!(&report.outcome, Outcome::Failed(reason) if reason.contains("rolled back")), "{:?}", report.outcome);
        assert_eq!((report.inserted, report.rolled_back, report.uncertain), (0, 10, 0));
        // No answer at all: it may have been applied, so it is not called rolled back.
        let mut silent = FakeWriter { commit_fails: Some(WriteFailure::Lost("connection reset".into())), ..Default::default() };
        let report = load_csv(&path, whole, &mut silent, NEVER);
        assert!(matches!(&report.outcome, Outcome::Failed(reason) if reason.contains("may or may not have been saved") && !reason.contains("rolled back")), "{:?}", report.outcome);
        assert_eq!((report.inserted, report.rolled_back, report.uncertain), (0, 0, 10));
    }

    #[test]
    fn a_partly_applied_batch_is_counted_never_retried_and_ends_the_load_even_when_skipping() {
        let (_dir, path) = csv_file(20, 0);
        for on_error in [OnError::Stop, OnError::SkipAndLog] {
            // The second batch (rows 5-8) wrote two rows, was cut off at a third and refused the fourth.
            let mut writer = FakeWriter { partial_at: Some((2, 2, 1)), ..Default::default() };
            let report = load_csv(&path, LoadSettings { on_error, ..PER_BATCH_STOP }, &mut writer, NEVER);
            assert!(matches!(&report.outcome, Outcome::Failed(reason) if reason.contains("only partly applied") && reason.contains("MyISAM")), "{:?}", report.outcome);
            assert_eq!(report.inserted, 4 + 2, "the first batch and what the partial one wrote");
            assert_eq!(report.uncertain, 1);
            assert_eq!(writer.calls, ["insert 4", "insert 4"], "no retry of the batch, nothing after it");
            assert_eq!(writer.ids(), [1, 2, 3, 4, 5, 6]);
        }
    }

    #[test]
    fn skipping_inside_one_transaction_still_commits_the_good_rows() {
        let (_dir, path) = csv_file(10, 4);
        let mut writer = FakeWriter::default();
        let settings = LoadSettings { transaction: TransactionMode::Whole, on_error: OnError::SkipAndLog, ..PER_BATCH_STOP };
        let report = load_csv(&path, settings, &mut writer, NEVER);
        assert_eq!(report.outcome, Outcome::Completed);
        assert_eq!((report.inserted, report.skipped), (8, 2));
        assert_eq!(writer.committed.len(), 8);
    }

    #[test]
    fn cancel_stops_between_rows_and_follows_the_transaction_mode() {
        let (_dir, path) = csv_file(100, 0);
        // Cancelled after the first batch went in: per batch keeps it, one transaction takes it back.
        for (mode, kept, rolled) in [(TransactionMode::PerBatch, 4, 0), (TransactionMode::Whole, 0, 4)] {
            let asked = AtomicBool::new(false);
            let calls = std::sync::atomic::AtomicUsize::new(0);
            let cancelled = |this: &AtomicBool| {
                // The check runs once per row: cancel while the second batch is being filled.
                if calls.fetch_add(1, Ordering::SeqCst) >= 5 {
                    this.store(true, Ordering::SeqCst);
                }
                this.load(Ordering::SeqCst)
            };
            let mut writer = FakeWriter::default();
            let settings = LoadSettings { transaction: mode, ..PER_BATCH_STOP };
            let report = load_csv(&path, settings, &mut writer, &|| cancelled(&asked));
            assert_eq!(report.outcome, Outcome::Cancelled, "{mode:?}");
            assert_eq!((writer.committed.len() as u64, report.inserted, report.rolled_back), (kept, kept, rolled), "{mode:?}");
        }
    }

    #[test]
    fn a_lost_connection_ends_the_load_whatever_the_policy() {
        let (_dir, path) = csv_file(20, 0);
        let mut writer = FakeWriter { lose_after_inserts: Some(1), ..Default::default() };
        let settings = LoadSettings { on_error: OnError::SkipAndLog, ..PER_BATCH_STOP };
        let report = load_csv(&path, settings, &mut writer, NEVER);
        assert_eq!(report.outcome, Outcome::Failed("connection lost".into()));
        assert_eq!(report.inserted, 4);
    }

    #[test]
    fn a_load_where_the_server_refuses_every_row_stops_instead_of_retrying_the_whole_file() {
        let (_dir, path) = csv_file(100, 0);
        let mut writer = FakeWriter { refuse_ids: (1..=100).collect(), ..Default::default() };
        let report = load_csv(&path, LoadSettings { on_error: OnError::SkipAndLog, ..PER_BATCH_STOP }, &mut writer, NEVER);
        assert!(matches!(&report.outcome, Outcome::Stopped(reason) if reason.contains("refused every row of the last 3 batches")), "{:?}", report.outcome);
        // Three batches of four, each sent whole and then row by row.
        assert_eq!(writer.calls.len(), 3 * (1 + 4));
    }

    #[test]
    fn the_error_log_is_capped_but_every_skip_is_counted() {
        let (_dir, path) = csv_file(ERROR_LOG_CAP + 150, 1);
        let mut writer = FakeWriter::default();
        let settings = LoadSettings { on_error: OnError::SkipAndLog, ..PER_BATCH_STOP };
        let report = load_csv(&path, settings, &mut writer, NEVER);
        assert_eq!(report.errors.len(), ERROR_LOG_CAP);
        assert_eq!((report.errors_total, report.skipped), ((ERROR_LOG_CAP + 150) as u64, (ERROR_LOG_CAP + 150) as u64));
        assert_eq!(report.inserted, 0);
    }

    #[test]
    fn empty_cells_are_null_in_typed_columns_and_text_in_text_columns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.csv");
        std::fs::write(&path, "id,name,score\n1,,\n2,x,3\n").unwrap();
        let mut writer = FakeWriter::default();
        let report = load_csv(&path, PER_BATCH_STOP, &mut writer, NEVER);
        assert_eq!(report.outcome, Outcome::Completed);
        assert_eq!(writer.committed[0]["name"], serde_json::json!(""));
        assert_eq!(writer.committed[0]["score"], Value::Null);
    }

    #[test]
    fn json_keys_after_the_preview_go_to_their_table_column_and_unknown_ones_follow_the_policy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.ndjson");
        std::fs::write(&path, "{\"id\": 1}\n{\"id\": 2, \"QTY\": \"5\"}\n{\"id\": 3, \"zzz\": 1}\n{\"id\": 4}\n").unwrap();
        let columns = [ColumnPlan { source: 0, target: "id".into(), converter: Converter::new("bigint") }];
        let extras = [ExtraColumn { key: "qty".into(), target: "qty".into(), converter: Converter::new("integer") }];
        let run_with = |on_error| {
            let progress = Progress::default();
            let mut source = SourceReader::open(&path, &SourceOptions::json(), &["id".to_owned()], progress.bytes.clone()).unwrap();
            let hints = Hints::default();
            let load = Load { columns: &columns, extras: &extras, hints: &hints, settings: LoadSettings { on_error, ..PER_BATCH_STOP }, progress: &progress, cancelled: NEVER, notify: &|| {} };
            let mut writer = FakeWriter::default();
            let report = run(&load, &mut source, &mut writer);
            (report, writer)
        };
        let (report, writer) = run_with(OnError::SkipAndLog);
        assert_eq!((report.inserted, report.skipped), (3, 1));
        assert_eq!(writer.committed[1]["qty"], serde_json::json!(5), "the key is a column of the table, by name");
        assert!(report.errors[0].message.contains("\"zzz\"") && report.errors[0].row == 3, "{:?}", report.errors);
        let (report, writer) = run_with(OnError::Stop);
        assert!(matches!(&report.outcome, Outcome::Stopped(reason) if reason.contains("zzz")), "{:?}", report.outcome);
        assert!(writer.ids().is_empty(), "the rows waiting in the batch when the load stops are not sent");
    }

    #[test]
    fn an_empty_cell_of_a_binary_column_is_loaded_as_null() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.csv");
        std::fs::write(&path, "id,data\n1,\n2,0x00ff\n").unwrap();
        let columns = [
            ColumnPlan { source: 0, target: "id".into(), converter: Converter::new("bigint") },
            ColumnPlan { source: 1, target: "data".into(), converter: Converter::new("varbinary(8)") },
        ];
        let progress = Progress::default();
        let mut source = SourceReader::open(&path, &SourceOptions::delimited(DelimitedFormat::default()), &[], progress.bytes.clone()).unwrap();
        let hints = Hints::default();
        let load = Load { columns: &columns, extras: &[], hints: &hints, settings: PER_BATCH_STOP, progress: &progress, cancelled: NEVER, notify: &|| {} };
        let mut writer = FakeWriter::default();
        let report = run(&load, &mut source, &mut writer);
        assert_eq!((report.outcome, report.inserted), (Outcome::Completed, 2));
        assert_eq!(writer.committed[0]["data"], Value::Null);
        assert_eq!(writer.committed[1]["data"], serde_json::json!("0x00ff"));
    }

    #[test]
    fn rows_of_the_wrong_width_follow_the_error_policy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.csv");
        std::fs::write(&path, "id,name,score\n1,a,1\n2,b\n3,c,3\n").unwrap();
        let mut stopped = FakeWriter::default();
        let report = load_csv(&path, PER_BATCH_STOP, &mut stopped, NEVER);
        assert!(matches!(&report.outcome, Outcome::Stopped(reason) if reason.contains("has 2 values, the header has 3")));
        let mut skipping = FakeWriter::default();
        let report = load_csv(&path, LoadSettings { on_error: OnError::SkipAndLog, ..PER_BATCH_STOP }, &mut skipping, NEVER);
        assert_eq!((report.inserted, report.skipped), (2, 1));
    }
}
