//! The run: the pieces of a [`ScriptReader`] in order into an [`Executor`], with the transaction mode, the
//! error policy, progress and cancel. Nothing here knows about a driver or a window.
//!
//! Memory is the statement being run (the reader holds one statement and a chunk), however large the file.
//! Statements are numbered from 1 in the order the file holds them, every piece counted (a statement, a psql line,
//! a `COPY … FROM stdin` block), so a number means the same thing in the next run of the same file: that is how a
//! run is resumed ([`Settings::start_at`]).
//!
//! Transactions:
//! * *autocommit*: each statement is its own step on the server, and a statement the file begins or ends a
//!   transaction with works as written;
//! * *whole file* and *every N statements*: the run opens the transaction (lazily, at the first statement it runs),
//!   commits it at the end (or every N statements) and rolls the open one back when the run stops, fails or is
//!   cancelled. The file's own `BEGIN` / `COMMIT` / `ROLLBACK` are left out then (reported as skipped): they would end the
//!   run's transaction under it.
//!
//! (MySQL and MariaDB commit by themselves before DDL: [`Settings::mysql`] makes the run start a new transaction
//! after each such statement, so a rollback never claims to undo what the server had already committed.)
//!
//! The first statement the server refuses ends a run in a transaction mode, whatever the policy says: PostgreSQL
//! refuses everything after it until the rollback, and there is no way to keep the rest of a batch.
//!
//! Where to start again ([`Report::resume_from`]): the statement that stopped an autocommit run, or the first
//! statement of the transaction that was rolled back (what came before it was committed and stays).

use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::head::{commits_implicitly, session_statement, transaction_of, Comments, SessionStatement, Transaction};
use crate::sql_script::{Piece, ScriptReader};

/// The most entries the report lists. More are counted, not kept.
pub(in crate::app) const ERROR_LOG_CAP: usize = 1000;

/// How often the run tells the window it moved.
pub(in crate::app) const NOTIFY_EVERY: Duration = Duration::from_millis(120);

/// Characters of a statement an entry shows.
const EXCERPT_CHARS: usize = 160;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum Transactions {
    /// Each statement is its own step.
    Autocommit,
    /// One transaction for everything that runs.
    Whole,
    /// A commit after every `n` statements that ran (`n` >= 1).
    Every(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum OnError {
    Stop,
    /// Log the statement and go on. Only for autocommit; a transaction mode stops anyway.
    Continue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) struct Settings {
    pub(in crate::app) transactions: Transactions,
    pub(in crate::app) on_error: OnError,
    /// The first statement to run (1-based): the ones before it are read and left out.
    pub(in crate::app) start_at: u64,
    /// MySQL and MariaDB: how the server reads a versioned comment. The server commits the open transaction by itself
    /// before DDL and a few other statements (also in a versioned comment): the run counts what came before such a
    /// statement as committed, and opens a new transaction after it. `None` for the other servers.
    pub(in crate::app) mysql: Option<Comments>,
}

impl Settings {
    fn managed(&self) -> bool {
        self.transactions != Transactions::Autocommit
    }
}

/// Why the executor cannot do what it was asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) enum Failure {
    /// The user cancelled the statement that was running.
    Cancelled,
    /// The server refused this statement. The session is fine.
    Rejected(String),
    /// The session cannot go on (the connection is gone, the driver refused the request).
    Lost(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => f.write_str("cancelled"),
            Self::Rejected(message) | Self::Lost(message) => f.write_str(message),
        }
    }
}

/// What a statement did, as far as the run cares.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(in crate::app) struct Done {
    /// It returned a result set (discarded).
    pub(in crate::app) result_set: bool,
    /// Rows a statement without a result set changed, when the driver says.
    pub(in crate::app) affected: u64,
}

/// What a psql line came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) enum CommandResult {
    /// Done by the run itself (`\connect`): the note says what.
    Done(String),
    /// Not run: it is for the psql client.
    Skipped(String),
    Failed(Failure),
}

/// Where the statements go. One call is one atomic step on the server.
pub(in crate::app) trait Executor {
    fn begin(&mut self) -> Result<(), Failure>;
    fn execute(&mut self, sql: &str) -> Result<Done, Failure>;
    /// A psql line (`\connect other`). `in_transaction`: the run holds a transaction open.
    fn command(&mut self, line: &str, in_transaction: bool) -> CommandResult;
    fn commit(&mut self) -> Result<(), Failure>;
    /// Ends the transaction without keeping anything; `false` when that could not be confirmed.
    fn rollback(&mut self) -> bool;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum EntryKind {
    Failed,
    Skipped,
    /// A statement before the start that only changes the session, run again so the start runs where it would have.
    Replayed,
    /// A `SET` before the start that is not run again (it changes the server, a transaction or an account, or its value is computed).
    NotRepeated,
}

/// One line of the list the summary shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) struct Entry {
    pub(in crate::app) number: u64,
    pub(in crate::app) line: u64,
    pub(in crate::app) excerpt: String,
    pub(in crate::app) message: String,
    pub(in crate::app) kind: EntryKind,
}

impl Entry {
    /// `#12 line 40: INSERT INTO t … -- the server's words`.
    pub(in crate::app) fn text(&self) -> String {
        format!("#{} line {}: {} \u{2014} {}", self.number, self.line, self.excerpt, self.message)
    }
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) enum Outcome {
    Completed,
    /// A statement was refused and the policy (or the transaction) says stop.
    Stopped(String),
    Cancelled,
    /// The file or the connection failed.
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) struct Report {
    pub(in crate::app) outcome: Outcome,
    pub(in crate::app) ok: u64,
    pub(in crate::app) failed: u64,
    /// Pieces the run did not send: psql lines, `COPY … FROM stdin` blocks, a transaction's own `BEGIN` / `COMMIT`.
    pub(in crate::app) skipped: u64,
    /// Statements before `start_at`, read and left out (the ones that set the session up are `replayed`).
    pub(in crate::app) passed_over: u64,
    /// Statements before `start_at` that were run again because they only change the session (`USE`, `SET`, `\connect`).
    pub(in crate::app) replayed: u64,
    /// `SET` statements before `start_at` that set something and were not run again (listed with the reason).
    pub(in crate::app) not_repeated: u64,
    /// Statements that returned a result set (discarded).
    pub(in crate::app) result_sets: u64,
    /// Rows the statements without a result set changed, as far as the driver said.
    pub(in crate::app) affected: u64,
    /// Statements the transaction took out again with its rollback (counted in `ok`).
    pub(in crate::app) rolled_back: u64,
    /// The number of the last statement the run reached.
    pub(in crate::app) last: u64,
    /// Where to start again so nothing that stands is run twice and nothing is missed.
    pub(in crate::app) resume_from: Option<u64>,
    /// The first [`ERROR_LOG_CAP`] failures, skips and replays.
    pub(in crate::app) entries: Vec<Entry>,
    pub(in crate::app) entries_total: u64,
    /// A COMMIT or ROLLBACK that was not confirmed: what the open transaction held may or may not have been kept.
    pub(in crate::app) uncertain: bool,
    pub(in crate::app) bytes: u64,
    pub(in crate::app) elapsed: Duration,
}

impl Report {
    pub(in crate::app) fn statements_per_second(&self) -> f64 {
        let seconds = self.elapsed.as_secs_f64();
        if seconds > 0.0 {
            (self.ok + self.failed) as f64 / seconds
        } else {
            0.0
        }
    }

    /// A report of a run that never got going.
    pub(in crate::app) fn failed(message: String, elapsed: Duration) -> Self {
        Self::new(Outcome::Failed(message), elapsed)
    }

    fn new(outcome: Outcome, elapsed: Duration) -> Self {
        Self {
            outcome,
            ok: 0,
            failed: 0,
            skipped: 0,
            passed_over: 0,
            replayed: 0,
            not_repeated: 0,
            result_sets: 0,
            affected: 0,
            rolled_back: 0,
            last: 0,
            resume_from: None,
            entries: Vec::new(),
            entries_total: 0,
            uncertain: false,
            bytes: 0,
            elapsed,
        }
    }
}

/// Counters the window reads while a run goes.
#[derive(Debug, Default)]
pub(in crate::app) struct Progress {
    /// The statement the run has reached.
    pub(in crate::app) number: AtomicU64,
    pub(in crate::app) ok: AtomicU64,
    pub(in crate::app) failed: AtomicU64,
    pub(in crate::app) skipped: AtomicU64,
    /// The file's bytes read so far, shared with the reader.
    pub(in crate::app) bytes: Arc<AtomicU64>,
}

impl Progress {
    pub(in crate::app) fn snapshot(&self) -> ProgressSnapshot {
        ProgressSnapshot {
            number: self.number.load(Ordering::Relaxed),
            ok: self.ok.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
            skipped: self.skipped.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(in crate::app) struct ProgressSnapshot {
    pub(in crate::app) number: u64,
    pub(in crate::app) ok: u64,
    pub(in crate::app) failed: u64,
    pub(in crate::app) skipped: u64,
    pub(in crate::app) bytes: u64,
}

/// What a run needs besides the reader and the executor.
pub(in crate::app) struct FileRun<'a> {
    pub(in crate::app) settings: Settings,
    pub(in crate::app) progress: &'a Progress,
    pub(in crate::app) cancelled: &'a (dyn Fn() -> bool + Sync),
    /// Called at most every [`NOTIFY_EVERY`], and once at the end.
    pub(in crate::app) notify: &'a (dyn Fn() + Sync),
}

/// Where the run's own COMMIT goes: after the statement that completes a batch, or before one that commits by itself.
#[derive(Clone, Copy)]
enum CommitAt {
    After,
    Before,
}

/// Why the loop stopped, and the statement a new run would start at (outside a transaction).
enum Ending {
    Done,
    Stopped(String, u64),
    Cancelled(u64),
    Failed(String, u64),
}

struct State<'a> {
    job: &'a FileRun<'a>,
    number: u64,
    /// A transaction of the run is open, with `in_tx` statements run in it since statement `tx_first`.
    open: bool,
    in_tx: u64,
    tx_first: u64,
    /// MySQL and MariaDB: the file set `autocommit` off, so a transaction is open after every commit (the next statement begins it).
    autocommit_off: bool,
    last_notified: Instant,
    report: Report,
}

/// Runs the file. Blocking: call it off the UI thread.
pub(in crate::app) fn run<R: Read, E: Executor>(job: &FileRun<'_>, reader: &mut ScriptReader<R>, executor: &mut E) -> Report {
    let started = Instant::now();
    let mut state = State { job, number: 0, open: false, in_tx: 0, tx_first: 0, autocommit_off: false, last_notified: started, report: Report::new(Outcome::Completed, Duration::ZERO) };
    let ending = state.drive(reader, executor);
    state.finish(ending, executor, started)
}

impl State<'_> {
    fn drive<R: Read, E: Executor>(&mut self, reader: &mut ScriptReader<R>, executor: &mut E) -> Ending {
        loop {
            let item = match reader.next() {
                Ok(Some(item)) => item,
                Ok(None) => return Ending::Done,
                Err(error) => return Ending::Failed(error.to_string(), self.number + 1),
            };
            self.number += 1;
            self.job.progress.number.store(self.number, Ordering::Relaxed);
            self.tick();
            if self.number < self.job.settings.start_at {
                if let Some(ending) = self.pass_over(&item, executor) {
                    return ending;
                }
                continue;
            }
            if (self.job.cancelled)() {
                return Ending::Cancelled(self.number);
            }
            self.report.last = self.number;
            let ending = match item.piece {
                Piece::Statement(_) => self.statement(item.text, item.line, executor),
                Piece::CopyFromStdin(_) => {
                    self.skip(item.text, item.line, "COPY … FROM stdin is not run: its data lines are in the file itself, and the server takes them only from a client. Load that table with Import Data.");
                    None
                }
                Piece::Command(_) => self.command(item.text, item.line, executor),
            };
            if let Some(ending) = ending {
                return ending;
            }
        }
    }

    /// A statement before the start: left out, unless it only changes the session, then it is run again.
    fn pass_over<E: Executor>(&mut self, item: &crate::sql_script::Item<'_>, executor: &mut E) -> Option<Ending> {
        let comments = self.job.settings.mysql;
        let repeat = match item.piece {
            Piece::Statement(_) => match session_statement(item.text, comments) {
                SessionStatement::Repeat => true,
                SessionStatement::NotRepeated(reason) => {
                    self.report.not_repeated += 1;
                    self.log(EntryKind::NotRepeated, item.text, item.line, format!("not repeated: {reason}"));
                    false
                }
                SessionStatement::Other => false,
            },
            Piece::Command(_) => item.text.starts_with(b"\\c ") || item.text.starts_with(b"\\connect"),
            Piece::CopyFromStdin(_) => false,
        };
        if !repeat {
            self.report.passed_over += 1;
            return None;
        }
        let restored = match (std::str::from_utf8(item.text), &item.piece) {
            (Ok(sql), Piece::Statement(_)) => executor.execute(sql).map(|_| ()),
            (Ok(command), _) => match executor.command(command, false) {
                CommandResult::Done(_) | CommandResult::Skipped(_) => Ok(()),
                CommandResult::Failed(failure) => Err(failure),
            },
            (Err(_), _) => Err(Failure::Rejected("it is not valid UTF-8".to_owned())),
        };
        match restored {
            Ok(()) => {
                self.report.replayed += 1;
                self.log(EntryKind::Replayed, item.text, item.line, "run again: it sets the session the start runs in".to_owned());
                None
            }
            Err(Failure::Cancelled) => Some(Ending::Cancelled(self.job.settings.start_at)),
            Err(failure) => Some(Ending::Failed(format!("The session the start runs in could not be set up: statement {} ({}) failed: {failure}", self.number, excerpt(item.text)), self.job.settings.start_at)),
        }
    }

    fn statement<E: Executor>(&mut self, text: &[u8], line: u64, executor: &mut E) -> Option<Ending> {
        let Ok(sql) = std::str::from_utf8(text) else {
            return self.refuse(text, line, "the statement is not valid UTF-8: choose the encoding of the file".to_owned());
        };
        let transaction = transaction_of(text, self.job.settings.mysql);
        let managed = self.job.settings.managed();
        if managed && transaction.is_some() {
            self.skip(text, line, "left out: the run keeps its own transaction (choose autocommit to run the file's BEGIN and COMMIT as written)");
            return None;
        }
        let commits = self.job.settings.mysql.is_some_and(|comments| commits_implicitly(text, comments));
        if commits {
            // The run commits the open transaction itself before the statement, its own or the file's: what it believes about
            // the transaction is what the server has, whether or not the server would have committed by itself (the guess
            // about the statement can be wrong, and a statement refused before it runs commits nothing).
            if self.open {
                if let Some(ending) = self.commit(executor, CommitAt::Before) {
                    return Some(ending);
                }
            }
            self.in_tx = 0;
        } else if let Some(ending) = self.open_transaction(executor) {
            return Some(ending);
        }
        match executor.execute(sql) {
            Ok(done) => {
                self.follow_file_transaction(transaction);
                // With autocommit off, the statement after it begins the next transaction.
                if commits && self.autocommit_off {
                    (self.open, self.in_tx, self.tx_first) = (true, 0, self.number + 1);
                }
                self.report.ok += 1;
                self.report.result_sets += u64::from(done.result_set);
                self.report.affected += done.affected;
                self.job.progress.ok.fetch_add(1, Ordering::Relaxed);
                // A statement the server commits before running is not in the transaction it ends, nor in the next one. In an
                // autocommit run, a statement that only sets the session is no work of the file's transaction either.
                let mysql = self.job.settings.mysql;
                if self.open && transaction.is_none() && !commits && (managed || mysql.is_none() || session_statement(text, mysql) == SessionStatement::Other) {
                    self.in_tx += 1;
                    if matches!(self.job.settings.transactions, Transactions::Every(every) if self.in_tx >= every) {
                        return self.commit(executor, CommitAt::After);
                    }
                }
                None
            }
            Err(Failure::Rejected(message)) => {
                let ending = self.refuse(text, line, message);
                // Going on after it with autocommit off: the statement after it begins the next transaction (a stop resumes at
                // this one, outside any transaction, as the run committed before it).
                if ending.is_none() && commits && self.autocommit_off {
                    (self.open, self.in_tx, self.tx_first) = (true, 0, self.number + 1);
                }
                ending
            }
            Err(Failure::Cancelled) => Some(Ending::Cancelled(self.number)),
            Err(Failure::Lost(message)) => Some(Ending::Failed(message, self.number)),
        }
    }

    /// The file's own transaction statements in an autocommit run: if the run ends inside the transaction they open, it is rolled
    /// back and the run resumes at its start.
    fn follow_file_transaction(&mut self, transaction: Option<Transaction>) {
        let number = self.number;
        match transaction {
            Some(Transaction::Begin) => (self.open, self.in_tx, self.tx_first) = (true, 0, number),
            Some(Transaction::End) => {
                (self.open, self.in_tx) = (false, 0);
                if self.autocommit_off {
                    (self.open, self.tx_first) = (true, number + 1);
                }
            }
            Some(Transaction::Chain) => (self.open, self.in_tx, self.tx_first) = (true, 0, number + 1),
            // Turning autocommit off commits nothing: a transaction already open goes on, else the next statement begins one.
            Some(Transaction::AutocommitOff) => {
                if !self.open {
                    (self.open, self.in_tx, self.tx_first) = (true, 0, number);
                }
                self.autocommit_off = true;
            }
            // The server commits only when autocommit goes from off to on; with it on already, a `BEGIN`'s transaction goes on.
            Some(Transaction::AutocommitOn) => {
                if self.autocommit_off {
                    (self.open, self.in_tx) = (false, 0);
                }
                self.autocommit_off = false;
            }
            None => {}
        }
    }

    fn command<E: Executor>(&mut self, text: &[u8], line: u64, executor: &mut E) -> Option<Ending> {
        let Ok(command) = std::str::from_utf8(text) else {
            return self.refuse(text, line, "the line is not valid UTF-8: choose the encoding of the file".to_owned());
        };
        match executor.command(command, self.open) {
            CommandResult::Done(_) => {
                self.report.ok += 1;
                self.job.progress.ok.fetch_add(1, Ordering::Relaxed);
                None
            }
            CommandResult::Skipped(note) => {
                self.skip(text, line, &note);
                None
            }
            CommandResult::Failed(Failure::Rejected(message)) => self.refuse(text, line, message),
            CommandResult::Failed(Failure::Cancelled) => Some(Ending::Cancelled(self.number)),
            CommandResult::Failed(Failure::Lost(message)) => Some(Ending::Failed(message, self.number)),
        }
    }

    fn open_transaction<E: Executor>(&mut self, executor: &mut E) -> Option<Ending> {
        if !self.job.settings.managed() || self.open {
            return None;
        }
        match executor.begin() {
            Ok(()) => {
                (self.open, self.in_tx, self.tx_first) = (true, 0, self.number);
                None
            }
            Err(Failure::Cancelled) => Some(Ending::Cancelled(self.number)),
            Err(Failure::Rejected(message)) => Some(Ending::Failed(format!("The transaction could not be opened: {message}"), self.number)),
            Err(Failure::Lost(message)) => Some(Ending::Failed(message, self.number)),
        }
    }

    /// COMMIT of the open transaction, sent after statement `number` or before it.
    fn commit<E: Executor>(&mut self, executor: &mut E, at: CommitAt) -> Option<Ending> {
        match executor.commit() {
            Ok(()) => {
                self.open = false;
                None
            }
            // The server refused the commit (a deferred constraint): it has rolled the transaction back.
            Err(Failure::Rejected(message)) => {
                self.report.rolled_back += std::mem::take(&mut self.in_tx);
                self.open = false;
                let at = match at {
                    CommitAt::After => "after",
                    CommitAt::Before => "before",
                };
                Some(Ending::Stopped(format!("the commit {at} statement {} was refused: {message}", self.number), self.tx_first))
            }
            Err(Failure::Cancelled) => {
                // Not reached: a COMMIT is not cancellable. Treated as unconfirmed.
                self.report.uncertain = true;
                self.open = false;
                Some(Ending::Failed("The commit was interrupted: the statements of the transaction may or may not be kept.".to_owned(), self.tx_first))
            }
            Err(Failure::Lost(message)) => {
                self.report.uncertain = true;
                self.open = false;
                Some(Ending::Failed(format!("{message} The commit was not confirmed: the statements of the transaction may or may not be kept."), self.tx_first))
            }
        }
    }

    /// The server refused statement `number`.
    fn refuse(&mut self, text: &[u8], line: u64, message: String) -> Option<Ending> {
        self.report.failed += 1;
        self.job.progress.failed.fetch_add(1, Ordering::Relaxed);
        self.log(EntryKind::Failed, text, line, message.clone());
        if self.job.settings.on_error == OnError::Continue && !self.job.settings.managed() {
            return None;
        }
        Some(Ending::Stopped(format!("statement {} was refused: {message}", self.number), self.number))
    }

    fn skip(&mut self, text: &[u8], line: u64, note: &str) {
        self.report.skipped += 1;
        self.job.progress.skipped.fetch_add(1, Ordering::Relaxed);
        self.log(EntryKind::Skipped, text, line, note.to_owned());
    }

    fn log(&mut self, kind: EntryKind, text: &[u8], line: u64, message: String) {
        self.report.entries_total += 1;
        if self.report.entries.len() < ERROR_LOG_CAP {
            self.report.entries.push(Entry { number: self.number, line, excerpt: excerpt(text), message, kind });
        }
    }

    fn tick(&mut self) {
        if self.last_notified.elapsed() >= NOTIFY_EVERY {
            self.last_notified = Instant::now();
            (self.job.notify)();
        }
    }

    fn finish<E: Executor>(mut self, ending: Ending, executor: &mut E, started: Instant) -> Report {
        let mut ending = ending;
        if matches!(ending, Ending::Done) && self.open {
            if self.job.settings.managed() {
                if let Some(committed) = self.commit(executor, CommitAt::After) {
                    ending = committed;
                }
            } else if self.in_tx > 0 {
                // The file ended inside a transaction of its own: the server rolls it back when the session closes.
                let note = format!("the file ends inside the transaction begun at statement {}, which was never committed: it is rolled back", self.tx_first);
                self.log(EntryKind::Skipped, b"(end of the file)", 0, note);
            }
        }
        // A transaction still open when the run ends early is rolled back: nothing of it is kept.
        let open_since = self.open.then_some(self.tx_first);
        if self.open {
            let rolled_back = executor.rollback();
            // An unconfirmed rollback of a transaction that holds no work (autocommit off, nothing run since) loses nothing.
            self.report.uncertain |= !rolled_back && self.in_tx > 0;
            self.report.rolled_back += std::mem::take(&mut self.in_tx);
            self.open = false;
        }
        // What was committed before the transaction that was rolled back stands: a new run starts at that transaction's
        // first statement. Without one open, at the statement the run was at.
        let resume = match &ending {
            Ending::Done => None,
            Ending::Stopped(_, at) | Ending::Cancelled(at) | Ending::Failed(_, at) => Some(open_since.unwrap_or(*at)),
        };
        self.report.outcome = match ending {
            Ending::Done => Outcome::Completed,
            Ending::Stopped(reason, _) => Outcome::Stopped(reason),
            Ending::Cancelled(_) => Outcome::Cancelled,
            Ending::Failed(message, _) => Outcome::Failed(message),
        };
        self.report.resume_from = resume.map(|at| at.max(self.job.settings.start_at));
        self.report.bytes = self.job.progress.bytes.load(Ordering::Relaxed);
        self.report.elapsed = started.elapsed();
        (self.job.notify)();
        self.report
    }
}

/// `text` on one line, at most [`EXCERPT_CHARS`] characters.
pub(in crate::app) fn excerpt(text: &[u8]) -> String {
    // A statement can be megabytes long: only its start is looked at.
    let head = &text[..text.len().min(EXCERPT_CHARS * 4)];
    let head = match std::str::from_utf8(head) {
        Ok(head) => std::borrow::Cow::Borrowed(head),
        Err(_) => String::from_utf8_lossy(head),
    };
    let mut line = String::new();
    let mut count = 0;
    for word in head.split_whitespace() {
        if count > 0 {
            line.push(' ');
            count += 1;
        }
        for c in word.chars() {
            if count >= EXCERPT_CHARS {
                line.push('\u{2026}');
                return line;
            }
            line.push(c);
            count += 1;
        }
    }
    if text.len() > head.len() {
        line.push('\u{2026}');
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql_script::Syntax;
    use engine_driver_kit::sql::SqlLexOptions;
    use std::collections::VecDeque;

    /// What the fake server was asked, and how it answers.
    #[derive(Default)]
    struct Fake {
        log: Vec<String>,
        /// Statements (by text) the server refuses.
        refuses: Vec<&'static str>,
        /// Statements that cancel the run when they are reached (the user pressed Cancel while it ran).
        cancels_at: Option<&'static str>,
        lost_at: Option<&'static str>,
        commit_answers: VecDeque<Result<(), Failure>>,
        rollback_confirmed: bool,
        cancel_flag: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    }

    impl Executor for Fake {
        fn begin(&mut self) -> Result<(), Failure> {
            self.log.push("BEGIN".into());
            Ok(())
        }
        fn execute(&mut self, sql: &str) -> Result<Done, Failure> {
            self.log.push(sql.to_owned());
            if self.refuses.contains(&sql) {
                return Err(Failure::Rejected(format!("refused: {sql}")));
            }
            if self.cancels_at == Some(sql) {
                return Err(Failure::Cancelled);
            }
            if self.lost_at == Some(sql) {
                return Err(Failure::Lost("connection lost".into()));
            }
            Ok(Done { result_set: sql.starts_with("select"), affected: u64::from(!sql.starts_with("select")) })
        }
        fn command(&mut self, line: &str, in_transaction: bool) -> CommandResult {
            self.log.push(format!("cmd {line}"));
            if line.starts_with("\\connect") {
                if in_transaction {
                    return CommandResult::Failed(Failure::Rejected("no switching inside a transaction".into()));
                }
                return CommandResult::Done("switched".into());
            }
            CommandResult::Skipped("psql only".into())
        }
        fn commit(&mut self) -> Result<(), Failure> {
            self.log.push("COMMIT".into());
            self.commit_answers.pop_front().unwrap_or(Ok(()))
        }
        fn rollback(&mut self) -> bool {
            self.log.push("ROLLBACK".into());
            self.rollback_confirmed
        }
    }

    fn fake() -> Fake {
        Fake { rollback_confirmed: true, ..Fake::default() }
    }

    fn go(script: &str, settings: Settings, fake: &mut Fake) -> Report {
        go_on(script, SqlLexOptions::postgres(), Syntax { psql_commands: true, ..Syntax::default() }, settings, fake)
    }

    fn go_on(script: &str, lex: SqlLexOptions, syntax: Syntax, settings: Settings, fake: &mut Fake) -> Report {
        let mut reader = ScriptReader::new(std::io::Cursor::new(script.as_bytes().to_vec()), lex, syntax);
        let progress = Progress::default();
        let flag = fake.cancel_flag.clone();
        let cancelled = move || flag.as_ref().is_some_and(|flag| flag.load(Ordering::SeqCst));
        let job = FileRun { settings, progress: &progress, cancelled: &cancelled, notify: &|| {} };
        run(&job, &mut reader, fake)
    }

    fn settings(transactions: Transactions, on_error: OnError, start_at: u64) -> Settings {
        Settings { transactions, on_error, start_at, mysql: None }
    }

    const AUTO_STOP: Settings = Settings { transactions: Transactions::Autocommit, on_error: OnError::Stop, start_at: 1, mysql: None };

    #[test]
    fn statements_run_in_order_each_on_its_own() {
        let mut server = fake();
        let report = go("create table t(a); insert into t values (1);\nselect * from t; insert into t values (2)", AUTO_STOP, &mut server);
        assert_eq!(report.outcome, Outcome::Completed);
        assert_eq!(server.log, ["create table t(a)", "insert into t values (1)", "select * from t", "insert into t values (2)"]);
        assert_eq!((report.ok, report.failed, report.skipped, report.result_sets, report.last, report.resume_from), (4, 0, 0, 1, 4, None));
        assert_eq!(report.affected, 3, "the rows the statements without a result set changed");
    }

    #[test]
    fn stop_ends_the_run_at_the_refused_statement_and_says_where_to_start_again() {
        let mut server = Fake { refuses: vec!["bad"], ..fake() };
        let report = go("one;\ntwo;\n\nbad;\nfour", AUTO_STOP, &mut server);
        assert_eq!(server.log, ["one", "two", "bad"], "nothing after the refused statement is sent");
        assert!(matches!(&report.outcome, Outcome::Stopped(reason) if reason.contains("statement 3") && reason.contains("refused: bad")), "{:?}", report.outcome);
        assert_eq!((report.ok, report.failed, report.last, report.resume_from), (2, 1, 3, Some(3)));
        assert_eq!(report.entries, [Entry { number: 3, line: 4, excerpt: "bad".into(), message: "refused: bad".into(), kind: EntryKind::Failed }]);
    }

    #[test]
    fn continue_logs_and_goes_on_and_the_log_is_capped() {
        let mut server = Fake { refuses: vec!["bad"], ..fake() };
        let report = go("one; bad; two; bad; three", settings(Transactions::Autocommit, OnError::Continue, 1), &mut server);
        assert_eq!(report.outcome, Outcome::Completed);
        assert_eq!((report.ok, report.failed, report.resume_from), (3, 2, None));
        assert_eq!(report.entries.iter().map(|entry| (entry.number, entry.line)).collect::<Vec<_>>(), [(2, 1), (4, 1)]);

        let script = "bad;".repeat(ERROR_LOG_CAP + 50);
        let mut server = Fake { refuses: vec!["bad"], ..fake() };
        let report = go(&script, settings(Transactions::Autocommit, OnError::Continue, 1), &mut server);
        assert_eq!((report.failed, report.entries_total, report.entries.len()), (1050, 1050, ERROR_LOG_CAP));
        assert_eq!(report.entries.last().unwrap().number, ERROR_LOG_CAP as u64, "the first ones are kept");
    }

    #[test]
    fn start_at_reads_the_first_statements_and_leaves_them_out() {
        let mut server = fake();
        let report = go("one; two; three; four", settings(Transactions::Autocommit, OnError::Stop, 3), &mut server);
        assert_eq!(server.log, ["three", "four"]);
        assert_eq!((report.passed_over, report.ok, report.last), (2, 2, 4));
    }

    #[test]
    fn a_resumed_run_numbers_statements_as_the_first_run_did() {
        let script = "-- header\nDELIMITER //\ncreate procedure p() begin select 1; select 2; end//\nDELIMITER ;\ninsert 1; insert 2; bad; insert 4";
        let mysql = |settings, server: &mut Fake| go_on(script, SqlLexOptions::mysql(), Syntax { delimiter_command: true, ..Syntax::default() }, settings, server);
        let mut first = Fake { refuses: vec!["bad"], ..fake() };
        let report = mysql(AUTO_STOP, &mut first);
        assert_eq!(report.resume_from, Some(4), "the procedure, two inserts, then the refused one");
        let mut second = fake();
        let again = mysql(settings(Transactions::Autocommit, OnError::Stop, 4), &mut second);
        assert_eq!(second.log, ["bad", "insert 4"]);
        assert_eq!((again.passed_over, again.last), (3, 5));
    }

    #[test]
    fn a_whole_file_transaction_commits_at_the_end_and_rolls_back_on_a_refusal() {
        let mut server = fake();
        let report = go("a; b; c", settings(Transactions::Whole, OnError::Stop, 1), &mut server);
        assert_eq!(server.log, ["BEGIN", "a", "b", "c", "COMMIT"]);
        assert_eq!((report.outcome.clone(), report.ok, report.rolled_back, report.resume_from), (Outcome::Completed, 3, 0, None));

        let mut server = Fake { refuses: vec!["c"], ..fake() };
        let report = go("a; b; c; d", settings(Transactions::Whole, OnError::Continue, 1), &mut server);
        assert_eq!(server.log, ["BEGIN", "a", "b", "c", "ROLLBACK"], "continue does not apply: the transaction is aborted");
        assert!(matches!(report.outcome, Outcome::Stopped(_)));
        assert_eq!((report.ok, report.failed, report.rolled_back, report.resume_from, report.uncertain), (2, 1, 2, Some(1), false));
    }

    #[test]
    fn every_n_commits_in_batches_and_resumes_at_the_batch_that_was_rolled_back() {
        let mut server = fake();
        let report = go("a; b; c; d; e", settings(Transactions::Every(2), OnError::Stop, 1), &mut server);
        assert_eq!(server.log, ["BEGIN", "a", "b", "COMMIT", "BEGIN", "c", "d", "COMMIT", "BEGIN", "e", "COMMIT"]);
        assert_eq!(report.outcome, Outcome::Completed);

        let mut server = Fake { refuses: vec!["d"], ..fake() };
        let report = go("a; b; c; d; e", settings(Transactions::Every(2), OnError::Stop, 1), &mut server);
        assert_eq!(server.log, ["BEGIN", "a", "b", "COMMIT", "BEGIN", "c", "d", "ROLLBACK"]);
        assert_eq!((report.ok, report.rolled_back, report.resume_from), (3, 1, Some(3)), "a and b are committed; c was rolled back with d's batch");
    }

    #[test]
    fn a_refused_commit_is_a_rolled_back_batch_and_a_lost_one_is_uncertain() {
        let mut server = Fake { commit_answers: VecDeque::from([Err(Failure::Rejected("deferred constraint".into()))]), ..fake() };
        let report = go("a; b", settings(Transactions::Whole, OnError::Stop, 1), &mut server);
        assert!(matches!(&report.outcome, Outcome::Stopped(reason) if reason.contains("deferred constraint")), "{:?}", report.outcome);
        assert_eq!((report.rolled_back, report.resume_from, report.uncertain), (2, Some(1), false));
        assert_eq!(server.log, ["BEGIN", "a", "b", "COMMIT"], "the server already rolled it back");

        let mut server = Fake { commit_answers: VecDeque::from([Err(Failure::Lost("gone".into()))]), ..fake() };
        let report = go("a", settings(Transactions::Whole, OnError::Stop, 1), &mut server);
        assert!(matches!(report.outcome, Outcome::Failed(_)));
        assert!(report.uncertain && report.rolled_back == 0);
    }

    #[test]
    fn the_files_own_transaction_statements_are_left_out_in_a_transaction_mode_only() {
        let script = "BEGIN TRANSACTION; insert 1; savepoint s; rollback to s; insert 2; COMMIT; -- done\nEND";
        let mut server = fake();
        let report = go(script, settings(Transactions::Whole, OnError::Stop, 1), &mut server);
        assert_eq!(server.log, ["BEGIN", "insert 1", "savepoint s", "rollback to s", "insert 2", "COMMIT"], "the savepoint and the rollback to it are the file's own work");
        assert_eq!((report.ok, report.skipped), (4, 3));
        assert!(report.entries.iter().all(|entry| entry.kind == EntryKind::Skipped && entry.message.contains("its own transaction")));

        let mut server = fake();
        go(script, AUTO_STOP, &mut server);
        assert_eq!(server.log[0], "BEGIN TRANSACTION");
        assert_eq!(server.log.len(), 7, "autocommit sends every statement as written");
    }

    #[test]
    fn cancel_stops_the_run_and_rolls_the_open_transaction_back() {
        let mut server = Fake { cancels_at: Some("c"), ..fake() };
        let report = go("a; b; c; d", settings(Transactions::Whole, OnError::Stop, 1), &mut server);
        assert_eq!(server.log, ["BEGIN", "a", "b", "c", "ROLLBACK"]);
        assert_eq!((report.outcome.clone(), report.rolled_back, report.resume_from), (Outcome::Cancelled, 2, Some(1)));

        // Between statements: the flag is looked at before each one.
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let mut server = Fake { cancel_flag: Some(flag), ..fake() };
        let report = go("a; b", AUTO_STOP, &mut server);
        assert!(server.log.is_empty());
        assert_eq!((report.outcome, report.resume_from), (Outcome::Cancelled, Some(1)));

        // Autocommit, cancelled while statement 2 ran: it is the one to run again.
        let mut server = Fake { cancels_at: Some("b"), ..fake() };
        let report = go("a; b; c", AUTO_STOP, &mut server);
        assert_eq!((report.outcome, report.ok, report.resume_from), (Outcome::Cancelled, 1, Some(2)));
    }

    #[test]
    fn a_lost_connection_fails_the_run_whatever_the_policy() {
        let mut server = Fake { lost_at: Some("b"), ..fake() };
        let report = go("a; b; c", settings(Transactions::Autocommit, OnError::Continue, 1), &mut server);
        assert_eq!(report.outcome, Outcome::Failed("connection lost".into()));
        assert_eq!((report.ok, report.resume_from), (1, Some(2)));
    }

    #[test]
    fn psql_lines_and_copy_blocks_are_reported_not_sent_and_connect_is_run() {
        let script = "\\connect other\ncreate table t(a);\nCOPY t FROM stdin;\n1\n2\n\\.\n\\set ON_ERROR_STOP on\ninsert into t values (3);";
        let mut server = fake();
        let report = go(script, AUTO_STOP, &mut server);
        assert_eq!(server.log, ["cmd \\connect other", "create table t(a)", "cmd \\set ON_ERROR_STOP on", "insert into t values (3)"], "the copy block never reaches the server");
        assert_eq!((report.ok, report.skipped, report.failed, report.last), (3, 2, 0, 5));
        let notes: Vec<_> = report.entries.iter().map(|entry| (entry.number, entry.message.as_str())).collect();
        assert!(notes[0].0 == 3 && notes[0].1.contains("Import Data"), "{notes:?}");
        assert_eq!(notes[1], (4, "psql only"));

        // \connect before the transaction opens switches; inside it, it is the executor's refusal, which stops the run.
        let mut server = fake();
        let report = go(script, settings(Transactions::Whole, OnError::Stop, 1), &mut server);
        assert_eq!(report.outcome, Outcome::Completed);
        assert_eq!(server.log[..2], ["cmd \\connect other", "BEGIN"]);
        let mut server = fake();
        let report = go("a;\n\\connect other\nb", settings(Transactions::Whole, OnError::Stop, 1), &mut server);
        assert!(matches!(&report.outcome, Outcome::Stopped(reason) if reason.contains("no switching")), "{:?}", report.outcome);
        assert_eq!((server.log.as_slice(), report.resume_from), (["BEGIN".to_owned(), "a".into(), "cmd \\connect other".into(), "ROLLBACK".into()].as_slice(), Some(1)));
    }

    #[test]
    fn a_statement_that_is_not_utf8_is_refused_with_a_hint_and_the_excerpt_is_short_and_on_one_line() {
        let mut reader = ScriptReader::new(std::io::Cursor::new(b"ok1;\nselect '\xff';\nok3".to_vec()), SqlLexOptions::postgres(), Syntax::default());
        let progress = Progress::default();
        let job = FileRun { settings: settings(Transactions::Autocommit, OnError::Continue, 1), progress: &progress, cancelled: &|| false, notify: &|| {} };
        let mut server = fake();
        let report = run(&job, &mut reader, &mut server);
        assert_eq!((report.ok, report.failed), (2, 1));
        assert!(report.entries[0].message.contains("not valid UTF-8") && report.entries[0].excerpt.starts_with("select '"), "{:?}", report.entries[0]);

        let long = format!("insert into t values\n   ({})", "1, ".repeat(500));
        let shown = excerpt(long.as_bytes());
        assert!(shown.starts_with("insert into t values (1, 1, 1,") && shown.ends_with('\u{2026}') && shown.chars().count() <= EXCERPT_CHARS + 1, "{shown}");
        assert_eq!(excerpt("é".repeat(100).as_bytes()).chars().count(), 100);
    }

    #[test]
    fn ddl_the_server_commits_by_itself_ends_the_transaction_the_run_has_open() {
        let mut server = Fake { refuses: vec!["bad"], ..fake() };
        let mysql = Settings { mysql: Some(Comments { version: Some(80_036), mariadb: false, ansi_quotes: false }), ..settings(Transactions::Whole, OnError::Stop, 1) };
        let report = go("create table t; insert 1; insert 2; alter table t; insert 3; bad; insert 4", mysql, &mut server);
        assert_eq!(server.log, ["create table t", "BEGIN", "insert 1", "insert 2", "COMMIT", "alter table t", "BEGIN", "insert 3", "bad", "ROLLBACK"]);
        assert_eq!((report.ok, report.rolled_back, report.resume_from), (5, 1, Some(5)), "insert 1 and 2 were committed by the ALTER; only insert 3 is undone");

        // Without the flag (PostgreSQL, SQLite) DDL is part of the transaction.
        let mut server = Fake { refuses: vec!["bad"], ..fake() };
        let report = go("create table t; insert 1; alter table t; bad", settings(Transactions::Whole, OnError::Stop, 1), &mut server);
        assert_eq!(server.log, ["BEGIN", "create table t", "insert 1", "alter table t", "bad", "ROLLBACK"]);
        assert_eq!((report.rolled_back, report.resume_from), (3, Some(1)));
    }

    #[test]
    fn a_commit_hidden_in_a_versioned_comment_is_still_a_commit() {
        // A mysqldump file in one transaction: the DISABLE KEYS block commits what came before it.
        let mut server = Fake { refuses: vec!["bad"], ..fake() };
        let mysql = Settings { mysql: Some(Comments { version: Some(80_036), mariadb: false, ansi_quotes: false }), ..settings(Transactions::Whole, OnError::Stop, 1) };
        let script = "/*!40101 SET x = 1 */; DROP TABLE IF EXISTS t; CREATE TABLE t(a); LOCK TABLES t WRITE; /*!40000 ALTER TABLE t DISABLE KEYS */; insert 1; /*!40000 ALTER TABLE t ENABLE KEYS */; UNLOCK TABLES; insert 2; bad";
        let report = go_on(script, SqlLexOptions::mysql().with_server_version(80_036), Syntax::default(), mysql, &mut server);
        assert_eq!(
            server.log,
            ["BEGIN", "/*!40101 SET x = 1 */", "COMMIT", "DROP TABLE IF EXISTS t", "CREATE TABLE t(a)", "LOCK TABLES t WRITE", "/*!40000 ALTER TABLE t DISABLE KEYS */", "BEGIN", "insert 1", "COMMIT", "/*!40000 ALTER TABLE t ENABLE KEYS */", "UNLOCK TABLES", "BEGIN", "insert 2", "bad", "ROLLBACK"]
        );
        assert_eq!((report.ok, report.rolled_back, report.resume_from), (9, 1, Some(9)), "only insert 2 was undone; insert 1 was committed by ENABLE KEYS");
    }

    #[test]
    fn the_run_commits_itself_before_a_statement_it_takes_for_a_commit_so_a_wrong_guess_costs_only_a_commit() {
        // `CREATE OR REPLACE TEMPORARY` and `LOAD DATA` are not commits, but a statement the run does take for one gets a COMMIT of the
        // run's own first: the transaction is then closed on the server too, and the accounting is what the table holds.
        let mut server = Fake { refuses: vec!["bad"], ..fake() };
        let mysql = Settings { mysql: Some(Comments { version: Some(80_036), mariadb: false, ansi_quotes: false }), ..settings(Transactions::Whole, OnError::Stop, 1) };
        let report = go_on("insert 1; insert 2; alter table t add x; insert 3; bad", SqlLexOptions::mysql(), Syntax::default(), mysql, &mut server);
        assert_eq!(server.log, ["BEGIN", "insert 1", "insert 2", "COMMIT", "alter table t add x", "BEGIN", "insert 3", "bad", "ROLLBACK"]);
        assert_eq!((report.ok, report.rolled_back, report.resume_from), (4, 1, Some(4)));
        // What is not taken for a commit stays in the transaction and is rolled back with it, however many rows it holds.
        let mut server = Fake { refuses: vec!["bad"], ..fake() };
        let script = format!("{} bad", "insert 1; ".repeat(1000));
        let report = go_on(&script, SqlLexOptions::mysql(), Syntax::default(), mysql, &mut server);
        assert_eq!((report.rolled_back, report.resume_from), (1000, Some(1)));
    }

    #[test]
    fn a_file_that_turns_autocommit_off_is_a_transaction_until_it_commits_and_after_every_commit() {
        let mysql = Settings { mysql: Some(Comments { version: Some(80_036), mariadb: false, ansi_quotes: false }), ..AUTO_STOP };
        let on = |script: &str, server: &mut Fake| go_on(script, SqlLexOptions::mysql(), Syntax::default(), mysql, server);
        // A --no-autocommit dump: the run stops inside the implicit transaction; what came before its COMMIT stands.
        let mut server = Fake { refuses: vec!["bad"], ..fake() };
        let report = on("SET autocommit=0; a; b; COMMIT; c; d; bad; e", &mut server);
        assert_eq!(server.log, ["SET autocommit=0", "a", "b", "COMMIT", "c", "d", "bad", "ROLLBACK"]);
        assert_eq!((report.ok, report.rolled_back, report.resume_from), (6, 2, Some(5)), "c and d are undone; the transaction began after the COMMIT, at statement 5");
        // DDL commits too, and the transaction after it is the next statement's.
        let mut server = Fake { refuses: vec!["bad"], ..fake() };
        let report = on("SET autocommit=0; a; create table t(x); b; bad", &mut server);
        assert_eq!((report.rolled_back, report.resume_from), (1, Some(4)));
        // Turned back on, it is the end of it.
        let mut server = Fake { refuses: vec!["bad"], ..fake() };
        let report = on("SET autocommit = 0; a; SET autocommit = 1; b; bad", &mut server);
        assert_eq!((report.rolled_back, report.resume_from), (0, Some(5)));
        // COMMIT AND CHAIN ends one transaction and begins the next.
        let mut server = Fake { refuses: vec!["bad"], ..fake() };
        let report = on("BEGIN; a; COMMIT AND CHAIN; b; bad", &mut server);
        assert_eq!((report.rolled_back, report.resume_from), (1, Some(4)));
        // In a transaction mode they are the run's to keep, and are left out like the other transaction statements.
        let mut server = fake();
        let managed = Settings { mysql: mysql.mysql, ..settings(Transactions::Whole, OnError::Stop, 1) };
        let report = go_on("SET autocommit=0; a; COMMIT AND CHAIN; b", SqlLexOptions::mysql(), Syntax::default(), managed, &mut server);
        assert_eq!(server.log, ["BEGIN", "a", "b", "COMMIT"]);
        assert_eq!(report.skipped, 2);
    }

    #[test]
    fn a_statement_taken_for_a_commit_in_an_autocommit_run_is_preceded_by_the_runs_commit_and_a_refusal_resumes_at_it() {
        let mysql = Settings { mysql: Some(Comments { version: Some(80_036), mariadb: false, ansi_quotes: false }), ..AUTO_STOP };
        let on = |script: &str, server: &mut Fake| go_on(script, SqlLexOptions::mysql(), Syntax::default(), mysql, server);
        // Autocommit off and a refused CREATE: what came before it is committed, and a new run starts at the CREATE.
        let mut server = Fake { refuses: vec!["create table t2(x)"], ..fake() };
        let report = on("SET autocommit=0; a; create table t2(x); b", &mut server);
        assert_eq!(server.log, ["SET autocommit=0", "a", "COMMIT", "create table t2(x)"]);
        assert_eq!((report.ok, report.rolled_back, report.resume_from), (2, 0, Some(3)));
        // Inside the file's BEGIN, a statement refused before it runs (a typo) would commit nothing on the server by itself: the
        // run's own COMMIT makes `a` stand, as the report says.
        let mut server = Fake { refuses: vec!["CREATE TABEL x(y)"], ..fake() };
        let report = on("BEGIN; a; CREATE TABEL x(y); b", &mut server);
        assert_eq!(server.log, ["BEGIN", "a", "COMMIT", "CREATE TABEL x(y)"]);
        assert_eq!((report.rolled_back, report.resume_from), (0, Some(3)));
        // Going on after the refusal with autocommit off, the next statement begins the next transaction.
        let continuing = Settings { on_error: OnError::Continue, ..mysql };
        let mut server = Fake { refuses: vec!["create table t2(x)"], cancels_at: Some("c"), ..fake() };
        let report = go_on("SET autocommit=0; a; create table t2(x); b; c", SqlLexOptions::mysql(), Syntax::default(), continuing, &mut server);
        assert_eq!((report.rolled_back, report.resume_from), (1, Some(4)), "b is undone with the transaction it began");
    }

    #[test]
    fn setting_autocommit_ends_the_files_transaction_only_when_it_turns_autocommit_on() {
        let mysql = Settings { mysql: Some(Comments { version: Some(80_036), mariadb: false, ansi_quotes: false }), ..AUTO_STOP };
        let on = |script: &str, server: &mut Fake| go_on(script, SqlLexOptions::mysql(), Syntax::default(), mysql, server);
        // With autocommit on already, `SET autocommit = 1` commits nothing: a and b are still the BEGIN's.
        let mut server = Fake { refuses: vec!["bad"], ..fake() };
        let report = on("BEGIN; a; SET autocommit=1; b; bad", &mut server);
        assert_eq!(server.log.last().map(String::as_str), Some("ROLLBACK"));
        assert_eq!((report.rolled_back, report.resume_from), (2, Some(1)));
        // Turning it off inside a transaction goes on with that transaction, and so does turning it off twice.
        let mut server = Fake { refuses: vec!["bad"], ..fake() };
        let report = on("BEGIN; a; SET autocommit=0; b; bad", &mut server);
        assert_eq!((report.rolled_back, report.resume_from), (2, Some(1)));
        let mut server = Fake { refuses: vec!["bad"], ..fake() };
        let report = on("SET autocommit=0; a; SET autocommit=0; b; bad", &mut server);
        assert_eq!((report.rolled_back, report.resume_from), (2, Some(1)));
    }

    #[test]
    fn a_no_autocommit_dump_that_ends_after_its_last_commit_ends_clean() {
        // mysqldump --no-autocommit ends with its COMMIT and the SETs that put the session back: nothing is left to roll back,
        // so there is no note, and an unconfirmed ROLLBACK of that empty transaction is not a doubt about the data.
        let mysql = Settings { mysql: Some(Comments { version: Some(80_036), mariadb: false, ansi_quotes: false }), ..AUTO_STOP };
        let mut server = Fake { rollback_confirmed: false, ..fake() };
        let report = go_on("SET autocommit=0; a; b; COMMIT; /*!40101 SET SQL_MODE=@OLD_SQL_MODE */; SET @x = 1", SqlLexOptions::mysql(), Syntax::default(), mysql, &mut server);
        assert_eq!(report.outcome, Outcome::Completed);
        assert_eq!((report.rolled_back, report.uncertain, report.entries.len()), (0, false, 0));
    }

    #[test]
    fn a_run_that_starts_late_repeats_what_sets_the_session_up_and_says_so() {
        let script = "\\connect db2\nSET search_path = a;\ncreate t; insert 1; set global x = 1; insert 2; insert 3";
        let mut server = fake();
        let report = go(script, settings(Transactions::Autocommit, OnError::Stop, 6), &mut server);
        assert_eq!(server.log, ["cmd \\connect db2", "SET search_path = a", "insert 2", "insert 3"], "the connect and the SET are run again; the table, the row and SET GLOBAL are not");
        assert_eq!((report.passed_over, report.replayed, report.ok, report.last), (3, 2, 2, 7), "the replays are not counted among the statements that ran");
        assert_eq!(report.entries.iter().map(|entry| (entry.number, entry.kind)).collect::<Vec<_>>(), [(1, EntryKind::Replayed), (2, EntryKind::Replayed), (5, EntryKind::NotRepeated)]);
        assert_eq!(report.not_repeated, 1);
        assert!(report.entries[2].message.contains("changes the server"), "{:?}", report.entries[2]);

        // A replay the server refuses ends the run before the start, which is where it would start again.
        let mut server = Fake { refuses: vec!["SET search_path = a"], ..fake() };
        let report = go(script, settings(Transactions::Autocommit, OnError::Continue, 6), &mut server);
        assert!(matches!(&report.outcome, Outcome::Failed(message) if message.contains("statement 2") && message.contains("could not be set up")), "{:?}", report.outcome);
        assert_eq!((report.ok, report.resume_from), (0, Some(6)));
        assert_eq!(server.log, ["cmd \\connect db2", "SET search_path = a"]);
    }

    #[test]
    fn the_files_own_transaction_in_an_autocommit_run_is_rolled_back_when_the_run_ends_inside_it() {
        let mut server = Fake { refuses: vec!["bad"], ..fake() };
        let report = go("a; BEGIN; b; c; bad; d", AUTO_STOP, &mut server);
        assert_eq!(server.log, ["a", "BEGIN", "b", "c", "bad", "ROLLBACK"]);
        assert_eq!((report.ok, report.rolled_back, report.resume_from), (4, 2, Some(2)), "b and c are undone; a stands; the file's BEGIN is where to start");
        let mut server = Fake { refuses: vec!["bad"], ..fake() };
        let report = go("BEGIN; b; COMMIT; c; bad", AUTO_STOP, &mut server);
        assert_eq!(server.log, ["BEGIN", "b", "COMMIT", "c", "bad"], "a transaction the file ended is not rolled back");
        assert_eq!((report.rolled_back, report.resume_from), (0, Some(5)));
        // A file that forgets its COMMIT: the session ends inside it.
        let mut server = fake();
        let report = go("BEGIN; b", AUTO_STOP, &mut server);
        assert_eq!(server.log, ["BEGIN", "b", "ROLLBACK"]);
        assert_eq!((report.outcome, report.rolled_back), (Outcome::Completed, 1));
        assert!(report.entries.iter().any(|entry| entry.message.contains("never committed")));
    }

    #[test]
    fn progress_counts_while_it_runs() {
        let mut server = Fake { refuses: vec!["bad"], ..fake() };
        let mut reader = ScriptReader::new(std::io::Cursor::new(b"a;\\q\nbad;b".to_vec()), SqlLexOptions::postgres(), Syntax { psql_commands: true, ..Syntax::default() });
        let progress = Progress::default();
        let job = FileRun { settings: settings(Transactions::Autocommit, OnError::Continue, 1), progress: &progress, cancelled: &|| false, notify: &|| {} };
        run(&job, &mut reader, &mut server);
        assert_eq!(progress.snapshot(), ProgressSnapshot { number: 4, ok: 2, failed: 1, skipped: 1, bytes: 0 });
    }
}
