//! Error Log model: a bounded in-memory ring of user-visible failures plus an
//! append-only JSON-lines file (`errors.log`, rotated) that gives the log
//! continuity across launches.
//!
//! This module is pure: no window, no executor. The app layer owns scheduling
//! (`app/error_log_view.rs` runs every file operation on the background
//! executor). Messages are redacted on the way in, so neither the viewer nor
//! the file ever holds a connection-string credential.

use gpui::SharedString;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// In-memory ring size; the oldest entry is dropped first.
pub const CAPACITY: usize = 1_000;
/// Entries restored from the file at startup.
pub const STARTUP_LOAD: usize = 200;
/// `errors.log` is rotated once an append would grow it past this.
pub const ROTATE_BYTES: u64 = 5 * 1024 * 1024;
/// Files kept on disk: `errors.log`, `errors.log.1`, `errors.log.2`.
pub const KEEP_FILES: usize = 3;
const FILE_NAME: &str = "errors.log";
/// `LoaderError::DriverRejected`'s `Display` prefix.
const DRIVER_REJECTED_PREFIX: &str = "driver rejected the request: ";
/// Driver codes whose rejection is documented as retryable.
const RETRYABLE_CODES: &[&str] = &["query.interrupted"];

/// The `<code>: <message>` tail of a stringified
/// `LoaderError::DriverRejected` (`driver rejected the request: ...`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DriverRejection<'a> {
    pub code: &'a str,
    /// The driver's message, when the text starts with the rejection (a
    /// wrapped rejection keeps its caller's context as the message instead).
    pub message_if_leading: Option<&'a str>,
}

impl<'a> DriverRejection<'a> {
    pub fn parse(text: &'a str) -> Option<Self> {
        let at = text.find(DRIVER_REJECTED_PREFIX)?;
        let rest = &text[at + DRIVER_REJECTED_PREFIX.len()..];
        let (code, message) = rest.split_once(": ")?;
        (!code.is_empty() && !code.contains(char::is_whitespace)).then_some(Self {
            code,
            message_if_leading: (at == 0).then_some(message),
        })
    }

    pub fn is_retryable(&self) -> bool {
        RETRYABLE_CODES.contains(&self.code)
    }

    pub fn is_cancelled(&self) -> bool {
        self.code.ends_with(".cancelled")
    }
}

#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warning,
    #[default]
    Error,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Self::Info => "Info",
            Self::Warning => "Warning",
            Self::Error => "Error",
        }
    }

    fn slot(self) -> usize {
        self as usize
    }
}

/// Where in the app the failure happened (the Source column).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Area {
    Query,
    Connect,
    Navigator,
    Mutation,
    Export,
    Driver,
    Admin,
    Diagram,
    #[default]
    App,
}

impl Area {
    pub fn label(self) -> &'static str {
        match self {
            Self::Query => "Query",
            Self::Connect => "Connect",
            Self::Navigator => "Navigator",
            Self::Mutation => "Mutation",
            Self::Export => "Export",
            Self::Driver => "Driver",
            Self::Admin => "Admin",
            Self::Diagram => "Diagram",
            Self::App => "App",
        }
    }
}

/// What a call site reports. Built with [`Report::error`] / [`Report::warning`]
/// / [`Report::info`] and the chained setters.
#[derive(Clone, Debug)]
pub struct Report {
    pub severity: Severity,
    pub area: Area,
    pub connection: Option<(String, String)>,
    pub code: Option<String>,
    pub retryable: bool,
    pub message: String,
    pub details: Option<String>,
}

impl Report {
    pub fn new(severity: Severity, area: Area, message: impl Into<String>) -> Self {
        Self {
            severity,
            area,
            connection: None,
            code: None,
            retryable: false,
            message: message.into(),
            details: None,
        }
    }

    #[cfg(test)]
    pub fn error(area: Area, message: impl Into<String>) -> Self {
        Self::new(Severity::Error, area, message)
    }

    #[cfg(test)]
    pub fn warning(area: Area, message: impl Into<String>) -> Self {
        Self::new(Severity::Warning, area, message)
    }

    #[cfg(test)]
    pub fn info(area: Area, message: impl Into<String>) -> Self {
        Self::new(Severity::Info, area, message)
    }

    /// Report for error text that may be a stringified driver rejection
    /// (see [`DriverRejection`]). The code becomes the entry's code; the
    /// retry hint is not in the text, so the documented retryable codes stand
    /// in for it. Cancellations are informational and retryable interrupts
    /// are warnings.
    pub fn from_text(severity: Severity, area: Area, text: &str) -> Self {
        let mut report = Self::new(severity, area, text);
        let rejection = DriverRejection::parse(text);
        if let Some(rejection) = &rejection {
            report.code = Some(rejection.code.to_string());
            if let Some(message) = rejection.message_if_leading {
                report.message = message.to_string();
            }
            if rejection.is_retryable() {
                report.retryable = true;
                report.severity = report.severity.min(Severity::Warning);
            }
        }
        if rejection.is_some_and(|rejection| rejection.is_cancelled())
            || text.contains("operation cancelled")
        {
            report.severity = Severity::Info;
        }
        report
    }

    #[cfg(test)]
    pub fn connection(mut self, id: impl Into<String>, name: impl Into<String>) -> Self {
        self.connection = Some((id.into(), name.into()));
        self
    }

    #[cfg(test)]
    pub fn code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }

    #[cfg(test)]
    pub fn retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }

    #[cfg(test)]
    pub fn details(mut self, details: impl Into<String>) -> Self {
        self.details = Some(details.into());
        self
    }
}

/// One row of the log. Display strings are built once, at insertion, so
/// rendering a row clones `SharedString`s and allocates nothing else.
#[derive(Clone, Debug)]
pub struct LogEntry {
    pub id: u64,
    pub at: SystemTime,
    pub severity: Severity,
    pub area: Area,
    pub connection_id: Option<String>,
    pub connection_name: Option<String>,
    pub code: Option<String>,
    pub retryable: bool,
    /// Redacted full text.
    pub message: SharedString,
    pub details: Option<String>,
    /// How many consecutive identical occurrences this row stands for.
    pub repeat: u32,
    /// First line of `message` (the table cell).
    pub first_line: SharedString,
    /// `connection · Area`, or just `Area`.
    pub source: SharedString,
    /// Local `YYYY-MM-DD HH:MM:SS.mmm` of the latest occurrence.
    pub time_text: SharedString,
    /// Lowercased message + source + code for the search box.
    search_key: String,
    unseen: bool,
}

pub fn format_time(at: SystemTime) -> String {
    chrono::DateTime::<chrono::Local>::from(at)
        .format("%Y-%m-%d %H:%M:%S%.3f")
        .to_string()
}

fn source_label(area: Area, connection_name: Option<&str>) -> String {
    match connection_name {
        Some(name) if !name.is_empty() => format!("{name} · {}", area.label()),
        _ => area.label().to_string(),
    }
}

impl LogEntry {
    fn build(id: u64, at: SystemTime, stored: StoredEntry) -> Self {
        let message = stored.message.trim_end().to_string();
        let message = if message.is_empty() {
            "Unknown error".to_string()
        } else {
            message
        };
        let source = source_label(stored.area, stored.connection_name.as_deref());
        let first_line = message.lines().next().unwrap_or("").trim().to_string();
        let search_key = format!(
            "{message}\n{source}\n{}",
            stored.code.as_deref().unwrap_or("")
        )
        .to_lowercase();
        Self {
            id,
            at,
            severity: stored.severity,
            area: stored.area,
            connection_id: stored.connection_id,
            connection_name: stored.connection_name,
            code: stored.code,
            retryable: stored.retryable,
            time_text: format_time(at).into(),
            first_line: first_line.into(),
            source: source.into(),
            message: message.into(),
            details: stored.details,
            repeat: stored.repeat.max(1),
            search_key,
            unseen: false,
        }
    }

    fn to_stored(&self) -> StoredEntry {
        StoredEntry {
            at_ms: self
                .at
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0),
            severity: self.severity,
            area: self.area,
            connection_id: self.connection_id.clone(),
            connection_name: self.connection_name.clone(),
            code: self.code.clone(),
            retryable: self.retryable,
            message: self.message.to_string(),
            details: self.details.clone(),
            repeat: self.repeat,
        }
    }

    /// Human-readable form used by Copy and Export.
    pub fn to_text(&self) -> String {
        let mut text = format!(
            "{} [{}] {}",
            self.time_text,
            self.severity.label().to_uppercase(),
            self.source
        );
        if let Some(code) = &self.code {
            text.push_str(&format!(" ({code})"));
        }
        if self.retryable {
            text.push_str(" retryable");
        }
        if self.repeat > 1 {
            text.push_str(&format!(" x{}", self.repeat));
        }
        text.push('\n');
        for line in self.message.lines() {
            text.push_str("  ");
            text.push_str(line);
            text.push('\n');
        }
        if let Some(details) = &self.details {
            for line in details.lines() {
                text.push_str("  ");
                text.push_str(line);
                text.push('\n');
            }
        }
        text
    }

    fn same_event(&self, other: &StoredEntry) -> bool {
        self.severity == other.severity
            && self.area == other.area
            && self.connection_id == other.connection_id
            && self.code == other.code
            && self.retryable == other.retryable
            && *self.message == *other.message.trim_end()
    }
}

/// On-disk shape of one JSON line. Every field defaults so older and newer
/// files degrade instead of failing.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StoredEntry {
    pub at_ms: i64,
    pub severity: Severity,
    pub area: Area,
    pub connection_id: Option<String>,
    pub connection_name: Option<String>,
    pub code: Option<String>,
    pub retryable: bool,
    pub message: String,
    pub details: Option<String>,
    pub repeat: u32,
}

impl StoredEntry {
    pub fn to_line(&self) -> String {
        let mut line = serde_json::to_string(self).unwrap_or_default();
        line.push('\n');
        line
    }
}

fn time_from_ms(ms: i64) -> SystemTime {
    if ms >= 0 {
        UNIX_EPOCH + Duration::from_millis(ms as u64)
    } else {
        UNIX_EPOCH
    }
}

/// What [`ErrorLog::record`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recorded {
    /// A new row was appended (the app persists it).
    Appended { id: u64 },
    /// The previous row absorbed an identical event (repeat count bumped).
    Repeated { id: u64 },
}

impl Recorded {
    #[cfg(test)]
    pub fn id(self) -> u64 {
        match self {
            Self::Appended { id } | Self::Repeated { id } => id,
        }
    }
}

/// Bounded log. Entries are oldest first; `revision` changes on every
/// mutation so view caches know when to rebuild.
#[derive(Debug, Default)]
pub struct ErrorLog {
    entries: VecDeque<LogEntry>,
    next_id: u64,
    unseen: [u32; 3],
    revision: u64,
}

impl ErrorLog {
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn get(&self, index: usize) -> Option<&LogEntry> {
        self.entries.get(index)
    }

    pub fn last(&self) -> Option<&LogEntry> {
        self.entries.back()
    }

    pub fn find(&self, id: u64) -> Option<&LogEntry> {
        self.entries.iter().rev().find(|entry| entry.id == id)
    }

    pub fn index_of(&self, id: u64) -> Option<usize> {
        self.entries.iter().rposition(|entry| entry.id == id)
    }

    fn fresh_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// Records an event at `now`. Redaction happens here, once.
    pub fn record(&mut self, report: Report, now: SystemTime) -> Recorded {
        let stored = StoredEntry {
            at_ms: now
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0),
            severity: report.severity,
            area: report.area,
            connection_id: report.connection.as_ref().map(|(id, _)| id.clone()),
            connection_name: report.connection.map(|(_, name)| name),
            code: report.code,
            retryable: report.retryable,
            message: crate::sessions::redact_secrets(&report.message),
            details: report
                .details
                .map(|details| crate::sessions::redact_secrets(&details)),
            repeat: 1,
        };
        self.revision += 1;
        if let Some(last) = self.entries.back_mut() {
            if last.same_event(&stored) {
                last.repeat = last.repeat.saturating_add(1);
                last.at = now;
                last.time_text = format_time(now).into();
                let id = last.id;
                let severity = last.severity;
                if !last.unseen {
                    last.unseen = true;
                    self.unseen[severity.slot()] += 1;
                }
                return Recorded::Repeated { id };
            }
        }
        let id = self.fresh_id();
        let mut entry = LogEntry::build(id, now, stored);
        entry.unseen = true;
        self.unseen[entry.severity.slot()] += 1;
        self.entries.push_back(entry);
        self.evict_overflow();
        Recorded::Appended { id }
    }

    fn evict_overflow(&mut self) {
        while self.entries.len() > CAPACITY {
            if let Some(old) = self.entries.pop_front() {
                if old.unseen {
                    self.unseen[old.severity.slot()] -= 1;
                }
            }
        }
    }

    /// Startup restore: older persisted entries go in front of anything
    /// recorded while the file was loading, already seen.
    pub fn prepend_loaded(&mut self, stored: Vec<StoredEntry>) {
        if stored.is_empty() {
            return;
        }
        let room = CAPACITY.saturating_sub(self.entries.len());
        let skip = stored.len().saturating_sub(room);
        let mut loaded = Vec::with_capacity(stored.len() - skip);
        for mut entry in stored.into_iter().skip(skip) {
            entry.message = crate::sessions::redact_secrets(&entry.message);
            let id = self.fresh_id();
            loaded.push(LogEntry::build(id, time_from_ms(entry.at_ms), entry));
        }
        for entry in loaded.into_iter().rev() {
            self.entries.push_front(entry);
        }
        self.revision += 1;
    }

    pub fn unseen_count(&self) -> u32 {
        self.unseen.iter().sum()
    }

    #[cfg(test)]
    pub fn unseen_of(&self, severity: Severity) -> u32 {
        self.unseen[severity.slot()]
    }

    /// Highest severity among unseen entries (drives the icon color).
    pub fn unseen_severity(&self) -> Option<Severity> {
        [Severity::Error, Severity::Warning, Severity::Info]
            .into_iter()
            .find(|severity| self.unseen[severity.slot()] > 0)
    }

    /// Newest unseen error or warning (the status bar text).
    pub fn latest_attention(&self) -> Option<&LogEntry> {
        self.entries
            .iter()
            .rev()
            .find(|entry| entry.unseen && entry.severity >= Severity::Warning)
    }

    pub fn mark_seen(&mut self) {
        if self.unseen_count() == 0 {
            return;
        }
        for entry in self.entries.iter_mut() {
            entry.unseen = false;
        }
        self.unseen = [0; 3];
        self.revision += 1;
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.unseen = [0; 3];
        self.revision += 1;
    }
}

// --- table view: filter + sort -------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SeverityFilter {
    #[default]
    All,
    Errors,
    Warnings,
    Info,
}

impl SeverityFilter {
    pub const ALL: [Self; 4] = [Self::All, Self::Errors, Self::Warnings, Self::Info];

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Errors => "Errors",
            Self::Warnings => "Warnings",
            Self::Info => "Info",
        }
    }

    fn matches(self, severity: Severity) -> bool {
        match self {
            Self::All => true,
            Self::Errors => severity == Severity::Error,
            Self::Warnings => severity == Severity::Warning,
            Self::Info => severity == Severity::Info,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SortKey {
    #[default]
    Date,
    Source,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewSpec {
    pub severity: SeverityFilter,
    /// Lowercased search text (empty = no search).
    pub query: String,
    pub sort: SortKey,
    /// Date: newest first. Source: Z..A.
    pub descending: bool,
}

impl Default for ViewSpec {
    fn default() -> Self {
        Self {
            severity: SeverityFilter::All,
            query: String::new(),
            sort: SortKey::Date,
            descending: true,
        }
    }
}

impl ErrorLog {
    /// Rebuilds the visible row order (indices into the log) into `out`,
    /// reusing its allocation.
    pub fn visible_indices(&self, spec: &ViewSpec, out: &mut Vec<usize>) {
        out.clear();
        out.extend(
            self.entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| {
                    spec.severity.matches(entry.severity)
                        && (spec.query.is_empty() || entry.search_key.contains(&spec.query))
                })
                .map(|(index, _)| index),
        );
        match (spec.sort, spec.descending) {
            // Entries are stored oldest first.
            (SortKey::Date, true) => out.reverse(),
            (SortKey::Date, false) => {}
            (SortKey::Source, descending) => out.sort_by(|a, b| {
                let by_source = self.entries[*a].source.cmp(&self.entries[*b].source);
                let by_source = if descending {
                    by_source.reverse()
                } else {
                    by_source
                };
                by_source.then(b.cmp(a))
            }),
        }
    }

    /// Export body: chronological, one block per entry.
    pub fn export_text(&self) -> String {
        self.entries.iter().map(LogEntry::to_text).collect()
    }
}

// --- persistence ----------------------------------------------------------

/// `<data dir>/tablex/logs`.
pub fn default_dir() -> Option<PathBuf> {
    crate::platform::app_data_dir().map(|dir| dir.join("logs"))
}

/// A queued file operation; the app drains them in order on one background
/// task, so file access is never concurrent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WriteOp {
    Append(String),
    DeleteAll,
}

/// The rotated JSON-lines file set. Plain blocking I/O: callers run it on a
/// background executor.
#[derive(Clone, Debug)]
pub struct LogStore {
    dir: PathBuf,
    rotate_bytes: u64,
}

impl LogStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            rotate_bytes: ROTATE_BYTES,
        }
    }

    #[cfg(test)]
    pub fn with_rotate_bytes(mut self, rotate_bytes: u64) -> Self {
        self.rotate_bytes = rotate_bytes;
        self
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join(FILE_NAME)
    }

    fn rotated(&self, index: usize) -> PathBuf {
        if index == 0 {
            self.path()
        } else {
            self.dir.join(format!("{FILE_NAME}.{index}"))
        }
    }

    /// Runs queued operations in order, merging adjacent appends.
    pub fn apply(&self, ops: Vec<WriteOp>) -> std::io::Result<()> {
        let mut pending = String::new();
        for op in ops {
            match op {
                WriteOp::Append(text) => pending.push_str(&text),
                WriteOp::DeleteAll => {
                    pending.clear();
                    self.delete_all()?;
                }
            }
        }
        if pending.is_empty() {
            Ok(())
        } else {
            self.append(&pending)
        }
    }

    pub fn append(&self, text: &str) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let size = std::fs::metadata(self.path()).map(|m| m.len()).unwrap_or(0);
        if size > 0 && size + text.len() as u64 > self.rotate_bytes {
            self.rotate()?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path())?;
        file.write_all(text.as_bytes())
    }

    fn rotate(&self) -> std::io::Result<()> {
        let _ = std::fs::remove_file(self.rotated(KEEP_FILES - 1));
        (0..KEEP_FILES - 1).rev().try_for_each(|index| {
            let from = self.rotated(index);
            if from.exists() {
                std::fs::rename(from, self.rotated(index + 1))
            } else {
                Ok(())
            }
        })
    }

    /// The newest `limit` entries across the file set, oldest first. Corrupt
    /// lines are skipped.
    pub fn load_recent(&self, limit: usize) -> Vec<StoredEntry> {
        // Newest file first; each file is parsed lazily, only until `limit`.
        let mut newest_first: Vec<StoredEntry> = (0..KEEP_FILES)
            .filter_map(|index| std::fs::read(self.rotated(index)).ok())
            .flat_map(|bytes| {
                String::from_utf8_lossy(&bytes)
                    .lines()
                    .rev()
                    .filter_map(|line| serde_json::from_str::<StoredEntry>(line).ok())
                    .collect::<Vec<_>>()
            })
            .take(limit)
            .collect();
        newest_first.reverse();
        newest_first
    }

    pub fn delete_all(&self) -> std::io::Result<()> {
        (0..KEEP_FILES).try_for_each(|index| {
            std::fs::remove_file(self.rotated(index)).or_else(|error| {
                (error.kind() == std::io::ErrorKind::NotFound)
                    .then_some(())
                    .ok_or(error)
            })
        })
    }
}

impl LogEntry {
    /// The line the app appends for a freshly recorded entry.
    pub fn to_line(&self) -> String {
        self.to_stored().to_line()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_700_000_000 + secs)
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("tablex-errlog-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn records_and_counts_unseen_by_severity() {
        let mut log = ErrorLog::default();
        log.record(Report::info(Area::App, "fyi"), at(0));
        assert_eq!(log.unseen_severity(), Some(Severity::Info));
        log.record(Report::warning(Area::Query, "slow"), at(1));
        assert_eq!(log.unseen_severity(), Some(Severity::Warning));
        log.record(Report::error(Area::Connect, "refused"), at(2));
        assert_eq!(log.unseen_severity(), Some(Severity::Error));
        assert_eq!(log.unseen_count(), 3);
        assert_eq!(
            log.latest_attention().unwrap().first_line.as_ref(),
            "refused"
        );
        log.mark_seen();
        assert_eq!(log.unseen_count(), 0);
        assert!(log.unseen_severity().is_none());
        assert!(log.latest_attention().is_none());
        assert_eq!(log.len(), 3);
    }

    #[test]
    fn consecutive_identical_events_collapse_with_a_repeat_count() {
        let mut log = ErrorLog::default();
        let first = log.record(
            Report::error(Area::Query, "boom").connection("c1", "Prod"),
            at(0),
        );
        let again = log.record(
            Report::error(Area::Query, "boom").connection("c1", "Prod"),
            at(5),
        );
        assert!(matches!(first, Recorded::Appended { .. }));
        assert_eq!(again, Recorded::Repeated { id: first.id() });
        assert_eq!(log.len(), 1);
        let entry = log.last().unwrap();
        assert_eq!(entry.repeat, 2);
        assert_eq!(entry.at, at(5), "row shows the latest occurrence");
        assert_eq!(log.unseen_count(), 1, "one unseen row, not one per repeat");
        // A different event in between breaks the run.
        log.record(Report::error(Area::Query, "other"), at(6));
        log.record(
            Report::error(Area::Query, "boom").connection("c1", "Prod"),
            at(7),
        );
        assert_eq!(log.len(), 3);
        // A different connection or code is a different event.
        log.record(
            Report::error(Area::Query, "boom").connection("c2", "Dev"),
            at(8),
        );
        log.record(
            Report::error(Area::Query, "boom")
                .connection("c2", "Dev")
                .code("x"),
            at(9),
        );
        assert_eq!(log.len(), 5);
    }

    #[test]
    fn repeat_of_a_seen_entry_lights_it_up_again() {
        let mut log = ErrorLog::default();
        log.record(Report::error(Area::Query, "boom"), at(0));
        log.mark_seen();
        log.record(Report::error(Area::Query, "boom"), at(1));
        assert_eq!(log.len(), 1);
        assert_eq!(log.unseen_of(Severity::Error), 1);
    }

    #[test]
    fn ring_is_bounded_and_eviction_keeps_unseen_counts_exact() {
        let mut log = ErrorLog::default();
        for i in 0..CAPACITY + 25 {
            log.record(Report::error(Area::Query, format!("e{i}")), at(i as u64));
        }
        assert_eq!(log.len(), CAPACITY);
        assert_eq!(log.get(0).unwrap().first_line.as_ref(), "e25");
        assert_eq!(log.unseen_count() as usize, CAPACITY);
        log.mark_seen();
        assert_eq!(log.unseen_count(), 0);
    }

    #[test]
    fn secrets_are_redacted_in_message_and_details() {
        let mut log = ErrorLog::default();
        log.record(
            Report::error(
                Area::Connect,
                "could not connect: postgres://alice:hunter2@db:5432/x refused",
            )
            .details("dial mongodb://bob:pw@h/db failed"),
            at(0),
        );
        let entry = log.last().unwrap();
        assert!(!entry.message.contains("hunter2"));
        assert!(!entry.message.contains("alice"));
        assert!(entry.message.contains("<redacted-uri>"));
        let details = entry.details.as_deref().unwrap();
        assert!(!details.contains("pw@") && !details.contains("bob"));
        assert!(!entry.to_line().contains("hunter2"));
    }

    #[test]
    fn first_line_source_and_time_are_precomputed() {
        let mut log = ErrorLog::default();
        log.record(
            Report::error(Area::Navigator, "first\nsecond line").connection("c", "Prod · pg"),
            at(0),
        );
        let entry = log.last().unwrap();
        assert_eq!(entry.first_line.as_ref(), "first");
        assert_eq!(entry.source.as_ref(), "Prod · pg · Navigator");
        assert_eq!(entry.time_text.len(), "2023-11-14 22:13:20.000".len());
        assert!(entry.to_text().contains("  second line"));
        let mut bare = ErrorLog::default();
        bare.record(Report::error(Area::Export, ""), at(0));
        assert_eq!(bare.last().unwrap().message.as_ref(), "Unknown error");
        assert_eq!(bare.last().unwrap().source.as_ref(), "Export");
    }

    #[test]
    fn filter_and_sort_rebuild_into_a_reused_buffer() {
        let mut log = ErrorLog::default();
        log.record(
            Report::error(Area::Query, "Alpha failed").connection("1", "Zed"),
            at(0),
        );
        log.record(
            Report::warning(Area::Driver, "beta retry")
                .connection("2", "Amy")
                .code("X1"),
            at(1),
        );
        log.record(Report::info(Area::Export, "gamma done"), at(2));
        let mut out = Vec::new();
        let mut spec = ViewSpec::default();
        log.visible_indices(&spec, &mut out);
        assert_eq!(out, vec![2, 1, 0], "newest first");
        spec.severity = SeverityFilter::Errors;
        log.visible_indices(&spec, &mut out);
        assert_eq!(out, vec![0]);
        spec.severity = SeverityFilter::All;
        spec.query = "x1".into();
        log.visible_indices(&spec, &mut out);
        assert_eq!(out, vec![1], "search covers the code, case-insensitively");
        spec.query = "amy".into();
        log.visible_indices(&spec, &mut out);
        assert_eq!(out, vec![1], "search covers the source");
        spec.query.clear();
        spec.sort = SortKey::Source;
        spec.descending = false;
        log.visible_indices(&spec, &mut out);
        let sources: Vec<_> = out
            .iter()
            .map(|i| log.get(*i).unwrap().source.to_string())
            .collect();
        assert_eq!(sources, vec!["Amy · Driver", "Export", "Zed · Query"]);
        spec.descending = true;
        log.visible_indices(&spec, &mut out);
        assert_eq!(out, vec![0, 2, 1]);
    }

    #[test]
    fn persistence_round_trips_and_keeps_the_newest_entries() {
        let dir = temp_dir("roundtrip");
        let store = LogStore::new(&dir);
        let mut log = ErrorLog::default();
        for i in 0..5u64 {
            let recorded = log.record(
                Report::error(Area::Query, format!("e{i}"))
                    .connection("c", "Prod")
                    .code("query.not_pageable")
                    .retryable(i == 4),
                at(i),
            );
            assert!(matches!(recorded, Recorded::Appended { .. }));
            store.append(&log.last().unwrap().to_line()).unwrap();
        }
        // A corrupt line is skipped, not fatal.
        store.append("{not json\n").unwrap();
        let loaded = store.load_recent(3);
        let messages: Vec<_> = loaded.iter().map(|e| e.message.as_str()).collect();
        assert_eq!(messages, vec!["e2", "e3", "e4"], "last N, chronological");
        let mut restored = ErrorLog::default();
        restored.record(Report::error(Area::App, "live"), at(100));
        restored.prepend_loaded(store.load_recent(STARTUP_LOAD));
        assert_eq!(restored.len(), 6);
        assert_eq!(restored.get(0).unwrap().first_line.as_ref(), "e0");
        assert_eq!(restored.last().unwrap().first_line.as_ref(), "live");
        assert_eq!(restored.unseen_count(), 1, "restored rows are already seen");
        let e4 = restored.get(4).unwrap();
        assert_eq!(e4.code.as_deref(), Some("query.not_pageable"));
        assert!(e4.retryable);
        assert_eq!(e4.at, at(4));
        assert_eq!(e4.connection_name.as_deref(), Some("Prod"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rotation_keeps_three_files_and_load_spans_them() {
        let dir = temp_dir("rotate");
        let store = LogStore::new(&dir).with_rotate_bytes(300);
        let mut log = ErrorLog::default();
        for i in 0..40u64 {
            log.record(Report::error(Area::Query, format!("entry-{i:02}")), at(i));
            store.append(&log.last().unwrap().to_line()).unwrap();
        }
        let names: std::collections::BTreeSet<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        let expected: std::collections::BTreeSet<_> =
            ["errors.log", "errors.log.1", "errors.log.2"]
                .into_iter()
                .map(String::from)
                .collect();
        assert_eq!(names, expected, "exactly three files are kept");
        for index in 0..KEEP_FILES {
            assert!(std::fs::metadata(store.rotated(index)).unwrap().len() <= 300);
        }
        let loaded = store.load_recent(1000);
        assert!(loaded.len() < 40, "oldest rotated files were dropped");
        assert_eq!(loaded.last().unwrap().message, "entry-39");
        let messages: Vec<_> = loaded.iter().map(|e| e.message.clone()).collect();
        let mut sorted = messages.clone();
        sorted.sort();
        assert_eq!(messages, sorted, "chronological across files");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn apply_merges_appends_and_delete_all_removes_every_file() {
        let dir = temp_dir("apply");
        let store = LogStore::new(&dir).with_rotate_bytes(100);
        let line = |m: &str| {
            let mut log = ErrorLog::default();
            log.record(Report::error(Area::App, m), at(0));
            log.last().unwrap().to_line()
        };
        store
            .apply(vec![WriteOp::Append(line("a")), WriteOp::Append(line("b"))])
            .unwrap();
        assert_eq!(store.load_recent(10).len(), 2);
        store
            .apply(vec![
                WriteOp::Append(line("c")),
                WriteOp::DeleteAll,
                WriteOp::Append(line("d")),
            ])
            .unwrap();
        let loaded = store.load_recent(10);
        assert_eq!(loaded.len(), 1, "appends before a delete are dropped");
        assert_eq!(loaded[0].message, "d");
        store.apply(vec![WriteOp::DeleteAll]).unwrap();
        assert!(store.load_recent(10).is_empty());
        assert!(!store.path().exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn export_text_is_chronological_and_complete() {
        let mut log = ErrorLog::default();
        log.record(
            Report::error(Area::Driver, "load failed\nsecond")
                .code("driver.rejected")
                .retryable(true),
            at(0),
        );
        log.record(Report::warning(Area::Query, "later"), at(1));
        let text = log.export_text();
        let first = text.find("load failed").unwrap();
        let later = text.find("later").unwrap();
        assert!(first < later);
        assert!(text.contains("[ERROR] Driver (driver.rejected) retryable"));
        assert!(text.contains("  second"));
        assert!(text.contains("[WARNING] Query"));
    }

    #[test]
    fn stringified_driver_rejections_yield_code_retryable_and_severity() {
        let report = Report::from_text(
            Severity::Error,
            Area::Query,
            "driver rejected the request: query.not_pageable: paging needs a sort",
        );
        assert_eq!(report.code.as_deref(), Some("query.not_pageable"));
        assert_eq!(report.message, "paging needs a sort");
        assert!(!report.retryable);
        assert_eq!(report.severity, Severity::Error);

        let interrupted = Report::from_text(
            Severity::Error,
            Area::Query,
            "driver rejected the request: query.interrupted: operation was interrupted",
        );
        assert!(interrupted.retryable);
        assert_eq!(interrupted.severity, Severity::Warning);

        let cancelled = Report::from_text(
            Severity::Error,
            Area::Query,
            "driver rejected the request: query.cancelled: Query cancelled",
        );
        assert_eq!(cancelled.severity, Severity::Info);
        assert_eq!(
            Report::from_text(Severity::Error, Area::Driver, "driver operation cancelled").severity,
            Severity::Info
        );

        let wrapped = Report::from_text(
            Severity::Error,
            Area::Navigator,
            "Could not list: driver rejected the request: session.not_connected: gone",
        );
        assert_eq!(wrapped.code.as_deref(), Some("session.not_connected"));
        assert!(
            wrapped.message.starts_with("Could not list"),
            "wrapper text is kept"
        );

        let plain = Report::from_text(Severity::Error, Area::Query, "syntax error at or near");
        assert!(plain.code.is_none());
    }

    #[test]
    fn clear_empties_the_log_and_resets_counters() {
        let mut log = ErrorLog::default();
        log.record(Report::error(Area::App, "x"), at(0));
        let revision = log.revision();
        log.clear();
        assert!(log.is_empty());
        assert_eq!(log.unseen_count(), 0);
        assert!(log.revision() > revision);
    }
}
