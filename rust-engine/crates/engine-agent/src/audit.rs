//! Append-only audit log of every tool call.
//!
//! One JSON object per line (JSONL):
//!
//! ```json
//! {"time":"2026-10-11T09:30:00.123Z","front_end":"mcp","connection_id":"c1","tool":"execute_query","statement":"SELECT 1","outcome":"ok","rows":1,"duration_ms":4}
//! ```
//!
//! * `outcome` is `ok` or the stable error code (`agent.write_refused`, ...).
//! * The line carries the statement text (secrets scrubbed by
//!   [`redact`](crate::redact::redact)) and a row *count*; never result data,
//!   URIs with credentials, passwords or error texts.
//! * **Off the hot path**: [`AuditLog::record`] only pushes the entry into a
//!   bounded queue; a writer thread serialises, appends and rotates. If the
//!   queue is full (a stalled disk) the entry is dropped and counted, and the
//!   writer later appends an `audit.dropped` line carrying the count.
//! * **Rotation**: when the active file would exceed `max_bytes` it becomes
//!   `<path>.1`, `.1` becomes `.2`, ... and the file beyond `max_files - 1` is
//!   deleted (default 5 files of 2 MiB: 1 active + 4 rotated).
//! * **Durability** ([`SyncPolicy`]): by default the writer hands each batch of
//!   entries to the OS (`write`, so a crash of this process loses nothing that
//!   was written) and `fsync`s when it rotates and when the log is closed. A
//!   power loss can lose the entries of the last batch; choose
//!   [`SyncPolicy::EveryBatch`] to `fsync` after each batch, at a latency
//!   cost for the writer thread only (never for the caller).
//! * **Symlinks:** the log path is opened with `O_NOFOLLOW` on Unix. A path whose
//!   last component is a symlink is refused (`AuditLog::open` fails; a link planted
//!   later makes the log unhealthy, so writes are refused), and permissions are
//!   never changed through a link.
//! * Several processes (the app and `tablex-mcp`) may append to one path:
//!   each line is one `O_APPEND` write. A rotation by one process leaves the
//!   other writing into the renamed file until it rotates too; nothing is lost.

use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;

/// Longest statement kept in a line; longer text is cut and marked.
pub const MAX_STATEMENT_BYTES: usize = 8 * 1024;
const QUEUE_CAPACITY: usize = 4096;
const OUTCOME_DROPPED: &str = "audit.dropped";

#[derive(Debug, Error)]
pub enum AuditError {
    #[error("audit log I/O failed on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("audit log configuration is invalid: {0}")]
    Config(&'static str),
}

/// When the writer `fsync`s.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SyncPolicy {
    /// On rotation and on close; entries are `write`n after each batch.
    #[default]
    OnRotateAndClose,
    /// After every batch of entries.
    EveryBatch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditConfig {
    /// Files kept, the active one included (`>= 1`).
    pub max_files: usize,
    /// Size at which the active file rotates.
    pub max_bytes: u64,
    pub sync: SyncPolicy,
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            max_files: 5,
            max_bytes: 2 * 1024 * 1024,
            sync: SyncPolicy::default(),
        }
    }
}

/// One line of the log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEntry {
    /// UTC, RFC 3339, millisecond precision.
    pub time: String,
    pub front_end: String,
    #[serde(default)]
    pub connection_id: Option<String>,
    pub tool: String,
    #[serde(default)]
    pub statement: Option<String>,
    pub outcome: String,
    #[serde(default)]
    pub rows: Option<u64>,
    pub duration_ms: u64,
    /// What the front end's approver decided for a write: `approved`, `declined: <reason>` or
    /// `cancelled`. Absent for everything that was not asked. (The model sees the error code
    /// only; this is for the person reading the log.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<String>,
}

impl AuditEntry {
    /// An entry stamped now.
    pub fn now(front_end: &str, tool: &str) -> Self {
        Self {
            time: rfc3339_millis(SystemTime::now()),
            front_end: front_end.to_owned(),
            connection_id: None,
            tool: tool.to_owned(),
            statement: None,
            outcome: String::new(),
            rows: None,
            duration_ms: 0,
            approval: None,
        }
    }
}

enum Message {
    Entry(Box<AuditEntry>),
    /// Write the entry, then say whether it reached the file.
    Acked(Box<AuditEntry>, mpsc::Sender<bool>),
    /// Reply once everything queued before it is written.
    Flush(mpsc::Sender<()>),
}

/// Handle to the writer thread. Cheap to share behind an `Arc`; dropping the
/// last handle drains the queue, `fsync`s and stops the thread.
pub struct AuditLog {
    path: PathBuf,
    sender: Mutex<Option<SyncSender<Message>>>,
    dropped: Arc<AtomicU64>,
    /// The last write reached the file (`false` while the disk or the path fails).
    healthy: Arc<AtomicBool>,
    tightened: bool,
    writer: Mutex<Option<JoinHandle<()>>>,
}

impl AuditLog {
    /// Opens (creating the parent directory and the file) the log at `path`.
    ///
    /// On Unix the file is created `0600` and directories this call creates
    /// `0700`; an existing file readable by group or others is tightened to
    /// `0600` ([`permissions_tightened`](Self::permissions_tightened) says so,
    /// and the first line written records it). On Windows the file inherits
    /// the ACL of the user profile directory it lives in.
    pub fn open(path: impl Into<PathBuf>, config: AuditConfig) -> Result<Self, AuditError> {
        if config.max_files == 0 || config.max_bytes == 0 {
            return Err(AuditError::Config(
                "max_files and max_bytes must be positive",
            ));
        }
        let path = path.into();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            create_private_dir(parent).map_err(|source| AuditError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let tightened = tighten_permissions(&path);
        let file = open_append(&path)?;
        let size = file
            .metadata()
            .map_err(|source| AuditError::Io {
                path: path.clone(),
                source,
            })?
            .len();
        let (sender, receiver) = mpsc::sync_channel::<Message>(QUEUE_CAPACITY);
        let dropped = Arc::new(AtomicU64::new(0));
        let healthy = Arc::new(AtomicBool::new(true));
        let mut writer = Writer {
            path: path.clone(),
            config,
            file: Some(file),
            size,
            dropped: Arc::clone(&dropped),
            healthy: Arc::clone(&healthy),
            note_tightened: tightened,
        };
        let handle = std::thread::Builder::new()
            .name("agent-audit-writer".into())
            .spawn(move || writer.run(receiver))
            .map_err(|source| AuditError::Io {
                path: path.clone(),
                source,
            })?;
        Ok(Self {
            path,
            sender: Mutex::new(Some(sender)),
            dropped,
            healthy,
            tightened,
            writer: Mutex::new(Some(handle)),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// An existing log file was readable by group or others and was tightened.
    pub fn permissions_tightened(&self) -> bool {
        self.tightened
    }

    /// The last write reached the file. `false` after a failed write, until a
    /// later one succeeds; entries lost meanwhile are counted
    /// ([`pending_dropped`](Self::pending_dropped)) and reported by the next
    /// successful write as an `audit.dropped` line.
    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    /// Queues `entry`; never blocks and never fails the caller.
    pub fn record(&self, entry: AuditEntry) {
        let guard = self
            .sender
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(sender) = guard.as_ref() else {
            return;
        };
        match sender.try_send(Message::Entry(Box::new(entry))) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    /// Writes `entry` and waits until it is in the file. Used before a write
    /// runs: if this fails the write must not run. Blocks the caller for one
    /// disk write (writes are rare and slow anyway).
    pub fn record_blocking(&self, entry: AuditEntry) -> Result<(), AuditError> {
        let (reply, wait) = mpsc::channel();
        let sent = {
            let guard = self
                .sender
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard
                .as_ref()
                .is_some_and(|sender| sender.send(Message::Acked(Box::new(entry), reply)).is_ok())
        };
        let unavailable = || AuditError::Io {
            path: self.path.clone(),
            source: io::Error::other("the audit log could not be written"),
        };
        if !sent {
            return Err(unavailable());
        }
        match wait.recv_timeout(Duration::from_secs(30)) {
            Ok(true) => Ok(()),
            _ => Err(unavailable()),
        }
    }

    /// Waits until everything recorded so far is written.
    pub fn flush(&self) {
        let (done, wait) = mpsc::channel();
        let sent = {
            let guard = self
                .sender
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard
                .as_ref()
                .is_some_and(|sender| sender.send(Message::Flush(done)).is_ok())
        };
        if sent {
            let _ = wait.recv_timeout(Duration::from_secs(30));
        }
    }

    /// Entries dropped (queue full, or a failed write), not yet reported in the file.
    pub fn pending_dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl Drop for AuditLog {
    fn drop(&mut self) {
        self.sender
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(handle) = self
            .writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            let _ = handle.join();
        }
    }
}

struct Writer {
    path: PathBuf,
    config: AuditConfig,
    /// `None` while the path cannot be opened.
    file: Option<File>,
    size: u64,
    dropped: Arc<AtomicU64>,
    healthy: Arc<AtomicBool>,
    /// Record once that an existing file's permissions were tightened.
    note_tightened: bool,
}

impl Writer {
    fn run(&mut self, receiver: mpsc::Receiver<Message>) {
        loop {
            let first = match receiver.recv_timeout(Duration::from_secs(1)) {
                Ok(message) => Some(message),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            };
            self.ensure_file();
            let mut flushes = Vec::new();
            let mut wrote = false;
            if self.note_tightened {
                self.note_tightened = false;
                let mut entry = AuditEntry::now("audit", "audit");
                entry.outcome = "audit.permissions_tightened".into();
                wrote |= self.append(&entry);
            }
            for message in first.into_iter().chain(receiver.try_iter()) {
                match message {
                    Message::Entry(entry) => wrote |= self.append(&entry),
                    Message::Acked(entry, reply) => {
                        // A batch keeps draining the queue without returning to the top of the
                        // loop, so the path may have changed since `ensure_file` ran: a blocking
                        // record is only acknowledged once it is in the file at the path.
                        self.ensure_file();
                        // Report earlier losses first, so the line order says when they happened.
                        wrote |= self.report_dropped();
                        let written = self.append(&entry);
                        wrote |= written;
                        let _ = reply.send(written);
                    }
                    Message::Flush(done) => flushes.push(done),
                }
            }
            wrote |= self.report_dropped();
            if wrote && self.config.sync == SyncPolicy::EveryBatch {
                if let Some(file) = &self.file {
                    let _ = file.sync_data();
                }
            }
            for done in flushes {
                let _ = done.send(());
            }
        }
        // Closed: write what is left, report drops, make it durable.
        self.ensure_file();
        for message in receiver.try_iter() {
            if let Message::Entry(entry) = message {
                self.append(&entry);
            }
        }
        self.report_dropped();
        if let Some(file) = &self.file {
            let _ = file.sync_all();
        }
    }

    /// Reopens the file if it was removed or replaced, or never opened.
    fn ensure_file(&mut self) {
        // `symlink_metadata`: a path that became a symlink is not our file any more.
        let present = fs::symlink_metadata(&self.path).is_ok_and(|metadata| metadata.is_file());
        if self.file.is_some() && present {
            return;
        }
        self.file = open_append(&self.path).ok();
        self.size = self
            .file
            .as_ref()
            .and_then(|file| file.metadata().ok())
            .map_or(0, |metadata| metadata.len());
    }

    fn report_dropped(&mut self) -> bool {
        if self.file.is_none() || self.dropped.load(Ordering::Relaxed) == 0 {
            return false;
        }
        let dropped = self.dropped.swap(0, Ordering::Relaxed);
        let mut entry = AuditEntry::now("audit", "audit");
        entry.outcome = OUTCOME_DROPPED.into();
        entry.rows = Some(dropped);
        let written = self.append(&entry);
        if !written {
            self.dropped.fetch_add(dropped, Ordering::Relaxed);
        }
        written
    }

    /// Writes one line, rotating first if it would not fit. Returns whether
    /// it was written; a failure is counted in `dropped` and flips `healthy`
    /// (a failing disk must not take the tool layer down).
    fn append(&mut self, entry: &AuditEntry) -> bool {
        let written = self.try_append(entry);
        self.healthy.store(written, Ordering::Relaxed);
        if !written && entry.outcome != OUTCOME_DROPPED {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        written
    }

    fn try_append(&mut self, entry: &AuditEntry) -> bool {
        let Ok(mut line) = serde_json::to_string(&clamp(entry)) else {
            return false;
        };
        line.push('\n');
        let length = line.len() as u64;
        if self.file.is_none() {
            return false;
        }
        if self.size > 0 && self.size + length > self.config.max_bytes {
            self.rotate();
        }
        let Some(file) = self.file.as_mut() else {
            return false;
        };
        if file.write_all(line.as_bytes()).is_err() {
            return false;
        }
        self.size += length;
        true
    }

    fn rotate(&mut self) {
        if let Some(file) = &self.file {
            let _ = file.sync_all();
        }
        let keep_rotated = self.config.max_files.saturating_sub(1);
        if keep_rotated == 0 {
            let _ = File::create(&self.path).map(|file| self.file = Some(file));
        } else {
            let _ = fs::remove_file(rotated(&self.path, keep_rotated));
            for index in (1..keep_rotated).rev() {
                let _ = fs::rename(rotated(&self.path, index), rotated(&self.path, index + 1));
            }
            let _ = fs::rename(&self.path, rotated(&self.path, 1));
        }
        if let Ok(file) = open_append(&self.path) {
            self.file = Some(file);
        } else {
            self.file = None;
        }
        self.size = self
            .file
            .as_ref()
            .and_then(|file| file.metadata().ok())
            .map_or(0, |metadata| metadata.len());
    }
}

fn clamp(entry: &AuditEntry) -> AuditEntry {
    let mut entry = entry.clone();
    if let Some(statement) = &entry.statement {
        if let Some(cut) = crate::limits::truncate_text(statement, MAX_STATEMENT_BYTES) {
            entry.statement = Some(cut);
        }
    }
    entry
}

/// Creates `dir` (and missing parents) `0700` on Unix.
fn create_private_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(dir)
    }
}

/// Tightens an existing log file that group or others can read. Returns
/// whether it changed anything.
fn tighten_permissions(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // `symlink_metadata`: never chmod through a symlink (it would change the target).
        let Ok(metadata) = fs::symlink_metadata(path) else {
            return false;
        };
        if !metadata.is_file() {
            return false;
        }
        if metadata.permissions().mode() & 0o077 == 0 {
            return false;
        }
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).is_ok()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

fn open_append(path: &Path) -> Result<File, AuditError> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // A path whose last component is a symlink is refused (ELOOP), so the log
        // can not be redirected into another file by planting a link.
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path).map_err(|source| AuditError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// `<path>.<index>`: the file name with the index appended.
fn rotated(path: &Path, index: usize) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{index}"));
    PathBuf::from(name)
}

/// What [`read_entries`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditRead {
    /// Newest first.
    pub entries: Vec<AuditEntry>,
    /// Lines that were not valid entries (a torn write, a foreign file).
    pub skipped_lines: usize,
}

/// Reads up to `max_entries` entries, newest first, across the active file
/// and the rotated ones. A missing file is an empty log.
pub fn read_entries(path: &Path, max_entries: usize) -> Result<AuditRead, AuditError> {
    let mut read = AuditRead::default();
    // Oldest rotated file first, so that reversing gives newest first.
    let mut files: Vec<PathBuf> = (1..=64)
        .map(|index| rotated(path, index))
        .take_while(|candidate| candidate.exists())
        .collect();
    files.reverse();
    files.push(path.to_path_buf());
    let mut all: Vec<AuditEntry> = Vec::new();
    for file in files {
        let handle = match File::open(&file) {
            Ok(handle) => handle,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(source) => return Err(AuditError::Io { path: file, source }),
        };
        for line in BufReader::new(handle).lines() {
            let line = line.map_err(|source| AuditError::Io {
                path: file.clone(),
                source,
            })?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<AuditEntry>(&line) {
                Ok(entry) => all.push(entry),
                Err(_) => read.skipped_lines += 1,
            }
        }
    }
    all.reverse();
    all.truncate(max_entries);
    read.entries = all;
    Ok(read)
}

/// `2026-10-11T09:30:00.123Z` for a point in time (UTC).
pub fn rfc3339_millis(time: SystemTime) -> String {
    let since_epoch = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let seconds = since_epoch.as_secs() as i64;
    let millis = since_epoch.subsec_millis();
    let (days, in_day) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        in_day / 3600,
        in_day % 3600 / 60,
        in_day % 60
    )
}

/// Days since 1970-01-01 -> (year, month, day), proleptic Gregorian
/// (Howard Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u32;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    } as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn timestamps_are_rfc3339_utc() {
        assert_eq!(rfc3339_millis(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            rfc3339_millis(UNIX_EPOCH + Duration::from_millis(1_760_175_000_123)),
            "2025-10-11T09:30:00.123Z"
        );
        // Leap day.
        assert_eq!(
            rfc3339_millis(UNIX_EPOCH + Duration::from_secs(1_709_164_800)),
            "2024-02-29T00:00:00.000Z"
        );
    }

    fn entry(n: usize) -> AuditEntry {
        let mut entry = AuditEntry::now("test", "execute_query");
        entry.connection_id = Some(format!("c{n}"));
        entry.statement = Some(format!("SELECT {n}"));
        entry.outcome = "ok".into();
        entry.rows = Some(n as u64);
        entry.duration_ms = 1;
        entry
    }

    #[test]
    fn entries_round_trip_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let log = AuditLog::open(&path, AuditConfig::default()).unwrap();
        for n in 0..5 {
            log.record(entry(n));
        }
        log.flush();
        let read = read_entries(&path, 3).unwrap();
        assert_eq!(read.skipped_lines, 0);
        assert_eq!(
            read.entries.iter().map(|e| e.rows).collect::<Vec<_>>(),
            [Some(4), Some(3), Some(2)]
        );
        assert_eq!(read.entries[0].statement.as_deref(), Some("SELECT 4"));
    }

    #[test]
    fn a_missing_log_reads_as_empty_and_garbage_lines_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("none.jsonl");
        assert_eq!(read_entries(&path, 10).unwrap(), AuditRead::default());
        fs::write(&path, "not json\n\n{\"half\":\n").unwrap();
        let read = read_entries(&path, 10).unwrap();
        assert!(read.entries.is_empty());
        assert_eq!(read.skipped_lines, 2);
    }

    #[test]
    fn the_log_rotates_by_size_and_keeps_the_configured_number_of_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let config = AuditConfig {
            max_files: 3,
            max_bytes: 600,
            sync: SyncPolicy::EveryBatch,
        };
        let log = AuditLog::open(&path, config).unwrap();
        for n in 0..60 {
            log.record(entry(n));
            if n % 7 == 0 {
                log.flush();
            }
        }
        log.flush();
        assert!(path.exists());
        assert!(rotated(&path, 1).exists());
        assert!(rotated(&path, 2).exists());
        assert!(
            !rotated(&path, 3).exists(),
            "only max_files - 1 rotated files are kept"
        );
        for file in [path.clone(), rotated(&path, 1), rotated(&path, 2)] {
            assert!(fs::metadata(&file).unwrap().len() <= 600, "{file:?}");
        }
        // The newest entry is first and the order is strictly decreasing.
        let read = read_entries(&path, 1000).unwrap();
        assert_eq!(read.entries[0].rows, Some(59));
        let rows: Vec<u64> = read.entries.iter().map(|e| e.rows.unwrap()).collect();
        assert!(rows.windows(2).all(|pair| pair[0] > pair[1]));
        assert!(rows.len() < 60, "the oldest files were deleted");
    }

    #[test]
    fn many_writers_lose_nothing_and_never_tear_a_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let log = Arc::new(AuditLog::open(&path, AuditConfig::default()).unwrap());
        let threads: Vec<_> = (0..8)
            .map(|t| {
                let log = Arc::clone(&log);
                std::thread::spawn(move || {
                    for n in 0..200 {
                        log.record(entry(t * 1000 + n));
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        log.flush();
        let read = read_entries(&path, 10_000).unwrap();
        assert_eq!(read.skipped_lines, 0);
        assert_eq!(read.entries.len(), 1600 - log.pending_dropped() as usize);
        let mut seen: Vec<u64> = read.entries.iter().map(|e| e.rows.unwrap()).collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), read.entries.len(), "no duplicates");
    }

    #[test]
    fn dropping_the_log_writes_what_is_queued() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        {
            let log = AuditLog::open(&path, AuditConfig::default()).unwrap();
            for n in 0..50 {
                log.record(entry(n));
            }
        }
        assert_eq!(read_entries(&path, 100).unwrap().entries.len(), 50);
    }

    #[test]
    fn long_statements_are_cut_and_the_file_reopens_in_append_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("audit.jsonl");
        {
            let log = AuditLog::open(&path, AuditConfig::default()).unwrap();
            let mut long = entry(1);
            long.statement = Some("x".repeat(50_000));
            log.record(long);
        }
        {
            let log = AuditLog::open(&path, AuditConfig::default()).unwrap();
            log.record(entry(2));
        }
        let read = read_entries(&path, 10).unwrap();
        assert_eq!(read.entries.len(), 2);
        let long = &read.entries[1];
        assert!(long.statement.as_ref().unwrap().len() < MAX_STATEMENT_BYTES + 100);
        assert!(long.statement.as_ref().unwrap().contains("[truncated"));
    }

    #[test]
    fn invalid_configuration_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let config = AuditConfig {
            max_files: 0,
            ..AuditConfig::default()
        };
        assert!(matches!(
            AuditLog::open(dir.path().join("a"), config),
            Err(AuditError::Config(_))
        ));
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[cfg(unix)]
    #[test]
    fn audit_file_mode_is_0600() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join("created")
            .join("deeper")
            .join("audit.jsonl");
        let config = AuditConfig {
            max_files: 3,
            max_bytes: 500,
            sync: SyncPolicy::OnRotateAndClose,
        };
        let log = AuditLog::open(&path, config).unwrap();
        for n in 0..30 {
            log.record(entry(n));
        }
        log.flush();
        assert_eq!(mode(&path), 0o600);
        assert_eq!(
            mode(path.parent().unwrap()),
            0o700,
            "directories this call created"
        );
        assert_eq!(mode(path.parent().unwrap().parent().unwrap()), 0o700);
        // Rotated files keep the mode (they are the renamed active file).
        assert!(rotated(&path, 1).exists());
        assert_eq!(mode(&rotated(&path, 1)), 0o600);
        assert!(!log.permissions_tightened());
    }

    #[cfg(unix)]
    #[test]
    fn an_existing_group_readable_file_is_tightened_and_noted() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        fs::write(&path, "").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let log = AuditLog::open(&path, AuditConfig::default()).unwrap();
        assert!(log.permissions_tightened());
        log.record(entry(1));
        log.flush();
        assert_eq!(mode(&path), 0o600);
        let entries = read_entries(&path, 10).unwrap().entries;
        assert!(entries
            .iter()
            .any(|e| e.outcome == "audit.permissions_tightened"));
        // An already private file is left alone and not reported.
        drop(log);
        let again = AuditLog::open(&path, AuditConfig::default()).unwrap();
        assert!(!again.permissions_tightened());
    }

    #[cfg(unix)]
    #[test]
    fn a_failing_log_is_unhealthy_refuses_blocking_records_and_reports_the_loss_later() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let log = AuditLog::open(&path, AuditConfig::default()).unwrap();
        log.record_blocking(entry(1)).unwrap();
        assert!(log.is_healthy());
        // The path becomes a directory: the file cannot be (re)opened.
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(log.record_blocking(entry(2)).is_err());
        assert!(!log.is_healthy());
        log.record(entry(3));
        log.flush();
        assert!(log.pending_dropped() >= 2, "{}", log.pending_dropped());
        // The path works again: the next record succeeds, and the loss is written first.
        fs::remove_dir(&path).unwrap();
        log.record_blocking(entry(4)).unwrap();
        assert!(log.is_healthy());
        log.flush();
        let entries = read_entries(&path, 10).unwrap().entries;
        let dropped = entries
            .iter()
            .find(|e| e.outcome == "audit.dropped")
            .expect("a dropped line");
        assert!(dropped.rows.unwrap() >= 2);
        assert!(entries.iter().any(|e| e.rows == Some(4)));
        assert_eq!(log.pending_dropped(), 0);
        // And the file is private again.
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn a_blocking_record_is_in_the_file_when_it_returns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let log = AuditLog::open(&path, AuditConfig::default()).unwrap();
        log.record_blocking(entry(7)).unwrap();
        assert_eq!(read_entries(&path, 10).unwrap().entries.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_log_path_is_refused_and_the_target_is_never_touched() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("victim.txt");
        fs::write(&target, "precious\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
        let link = dir.path().join("audit.jsonl");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let error = AuditLog::open(&link, AuditConfig::default())
            .err()
            .expect("refused");
        assert!(matches!(error, AuditError::Io { .. }), "{error:?}");
        // Neither written to nor chmod-ed through the link.
        assert_eq!(fs::read_to_string(&target).unwrap(), "precious\n");
        assert_eq!(mode(&target), 0o644);
        // A dangling link is refused too (it must not be created through).
        let dangling = dir.path().join("dangling.jsonl");
        std::os::unix::fs::symlink(dir.path().join("created-by-attack"), &dangling).unwrap();
        assert!(AuditLog::open(&dangling, AuditConfig::default()).is_err());
        assert!(!dir.path().join("created-by-attack").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_log_replaced_by_a_symlink_while_running_turns_unhealthy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let target = dir.path().join("victim.txt");
        fs::write(&target, "precious\n").unwrap();
        let log = AuditLog::open(&path, AuditConfig::default()).unwrap();
        log.record_blocking(entry(1)).unwrap();
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(log.record_blocking(entry(2)).is_err());
        assert!(!log.is_healthy());
        assert_eq!(fs::read_to_string(&target).unwrap(), "precious\n");
        // Removing the link lets the log recover.
        fs::remove_file(&path).unwrap();
        log.record_blocking(entry(3)).unwrap();
        assert!(log.is_healthy());
    }
}
