//! GPUI workspace session: the tabs the shell had open, persisted to its own
//! file so a relaunch restores order, pins, the active tab, per-tab text and
//! table filters, and collapsed tab groups.
//!
//! The file is `<data dir>/tablex/gpui-workspace-session.json`.
//! The Swift app's `workspace-session.json` is never written (see `session.rs`,
//! which stays a read-only import used when this file does not exist).
//!
//! Format: versioned JSON with `serde(default)` on every field so older and
//! newer files degrade instead of failing. Writes are atomic (temp file in the
//! same directory, then rename). A corrupted file is reported to the caller
//! and never aborts startup.

use crate::query_mode::QueryMode;
use serde::{Deserialize, Serialize};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

/// Bump when the schema changes incompatibly; readers refuse newer files.
pub const SESSION_VERSION: u32 = 1;
const FILE_NAME: &str = "gpui-workspace-session.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionTab {
    /// `welcome`, `query`, `connection`, `diagram` or `source`.
    pub kind: String,
    pub title: String,
    pub connection_id: String,
    pub database: String,
    pub schema: Option<String>,
    pub collection: String,
    pub query_mode: Option<QueryMode>,
    pub is_pinned: bool,
    pub sql: String,
    pub output_limit: i64,
    /// Applied WHERE / ORDER BY of a `sqlTable` tab (what paging re-runs).
    pub table_where: String,
    pub table_order_by: String,
    /// Unapplied editor text, stored only when it differs from the applied one.
    pub table_where_draft: Option<String>,
    pub table_order_by_draft: Option<String>,
    /// Filter-builder rules of a `sqlTable` tab (drafts included) and the
    /// WHERE text the builder last wrote (to detect a manual edit).
    pub table_filter_rules: Vec<crate::sql_filter::SqlFilterRule>,
    pub table_filter_where: Option<String>,
    /// Result columns hidden in the grid of a `sqlTable` tab.
    pub table_hidden_columns: Vec<String>,
    /// Result columns pinned to the left of the grid, in pin order.
    pub pinned_columns: Vec<crate::pinned_column::PinnedColumn>,
    /// The `.sql` file the tab is bound to.
    pub file_path: Option<String>,
    /// The file's text when the tab differs from it (else the tab's own text is the file's).
    pub file_saved: Option<String>,
    /// Pipeline of a `mongoAggregate` tab.
    pub aggregate: Option<SessionAggregate>,
    /// The schema object a `source` tab shows (its text is fetched again).
    pub source: Option<engine_driver_api::sql::SchemaObjectRef>,
}

/// What an Aggregate tab persists: the enabled pipeline text, the editor
/// mode and toggle, options (disabled stages included) and saved identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionAggregate {
    pub mode: crate::aggregate::EditorMode,
    pub preview: bool,
    pub text: String,
    pub options: crate::aggregate::AggregateOptions,
    pub name: Option<String>,
    pub saved_id: Option<String>,
}

impl Default for SessionAggregate {
    fn default() -> Self {
        Self {
            mode: crate::aggregate::EditorMode::Text,
            preview: true,
            text: "[]".into(),
            options: Default::default(),
            name: None,
            saved_id: None,
        }
    }
}

impl Default for SessionTab {
    fn default() -> Self {
        Self {
            kind: "query".into(),
            title: String::new(),
            connection_id: String::new(),
            database: String::new(),
            schema: None,
            collection: String::new(),
            query_mode: None,
            is_pinned: false,
            sql: String::new(),
            output_limit: crate::query::DEFAULT_OUTPUT_LIMIT,
            table_where: String::new(),
            table_order_by: String::new(),
            table_where_draft: None,
            table_order_by_draft: None,
            table_filter_rules: Vec::new(),
            table_filter_where: None,
            table_hidden_columns: Vec::new(),
            pinned_columns: Vec::new(),
            file_path: None,
            file_saved: None,
            aggregate: None,
            source: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SessionGroup {
    pub connection_id: String,
    pub database: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GpuiSession {
    pub version: u32,
    /// Tabs in bar order.
    pub tabs: Vec<SessionTab>,
    /// Index into `tabs` of the active tab (of the focused pane).
    pub active: Option<usize>,
    pub collapsed_groups: Vec<SessionGroup>,
    /// Pane layout (tabs by index into `tabs`, split ratios, per-pane active
    /// tab and collapsed groups). Absent for one pane and in sessions that
    /// predate panes: every tab then lives in a single pane, `active` and
    /// `collapsed_groups` apply to it.
    pub panes: Option<crate::panes::PaneLayoutSpec>,
}

impl Default for GpuiSession {
    fn default() -> Self {
        Self {
            version: SESSION_VERSION,
            tabs: Vec::new(),
            active: None,
            collapsed_groups: Vec::new(),
            panes: None,
        }
    }
}

/// What reading the session file produced.
#[derive(Debug)]
pub enum LoadOutcome {
    /// No file: callers fall back to the Swift import.
    Missing,
    Loaded(GpuiSession),
    /// The file exists but cannot be used; the message is user-facing.
    Unusable(String),
}

/// `<data dir>/tablex/gpui-workspace-session.json`.
pub fn default_path() -> Option<PathBuf> {
    crate::platform::app_data_dir().map(|dir| dir.join(FILE_NAME))
}

pub fn load(path: &Path) -> LoadOutcome {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return LoadOutcome::Missing,
        Err(error) => {
            return LoadOutcome::Unusable(format!("Could not read the saved workspace: {error}"))
        }
    };
    match serde_json::from_str::<GpuiSession>(&text) {
        Ok(session) if session.version > SESSION_VERSION => LoadOutcome::Unusable(format!(
            "Saved workspace was written by a newer version (format {})",
            session.version
        )),
        Ok(session) => LoadOutcome::Loaded(session),
        Err(error) => LoadOutcome::Unusable(format!(
            "Saved workspace is corrupted and was ignored: {error}"
        )),
    }
}

/// Moves an unusable file aside (best effort) so the next save does not
/// destroy it silently.
pub fn quarantine(path: &Path) {
    let mut name = path.as_os_str().to_owned();
    name.push(".corrupt");
    let _ = std::fs::rename(path, PathBuf::from(name));
}

/// Serializes once; the caller may hand the bytes to a background thread.
pub fn encode(session: &GpuiSession) -> Result<Vec<u8>, String> {
    serde_json::to_vec_pretty(session).map_err(|error| error.to_string())
}

/// Distinguishes the temp files of concurrent writes inside one process.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Atomic write: temp file next to the target, flushed, then renamed over it.
/// The temp name is unique per call (pid + monotonic counter), so concurrent
/// writers never share or truncate each other's temp file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.tmp-{}-{}",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(FILE_NAME),
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What a [`SessionWriter`] write did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    Written,
    /// Nothing to do: the file already holds these bytes, or a newer
    /// snapshot was written first (an older one must never overwrite it).
    Skipped,
}

#[derive(Default)]
struct Submitted {
    ticket: u64,
    /// Latest snapshot handed to a writer and not known to have failed.
    bytes: Option<Vec<u8>>,
}

#[derive(Default)]
struct OnDisk {
    /// Ticket of the snapshot currently on disk.
    ticket: u64,
    /// Bytes last written successfully.
    bytes: Option<Vec<u8>>,
}

/// Single ordered writer for the session file. Every snapshot gets a ticket;
/// the file is only ever replaced by a snapshot newer than the one on disk,
/// and writes run one at a time, so a debounce write that finishes after a
/// quit-time write can no longer clobber it, and the "already saved" memory
/// is only set once the bytes really are on disk.
#[derive(Default)]
pub struct SessionWriter {
    next_ticket: AtomicU64,
    submitted: Mutex<Submitted>,
    io: Mutex<OnDisk>,
}

impl SessionWriter {
    /// Reserves the next ticket for `bytes`. `None` when identical bytes were
    /// already submitted and have not failed (nothing new to write).
    pub fn submit(&self, bytes: &[u8]) -> Option<u64> {
        let mut submitted = locked(&self.submitted);
        if submitted.bytes.as_deref() == Some(bytes) {
            return None;
        }
        let ticket = self.next_ticket.fetch_add(1, Ordering::SeqCst) + 1;
        submitted.ticket = ticket;
        submitted.bytes = Some(bytes.to_vec());
        Some(ticket)
    }

    /// Writes the snapshot of `ticket` (background path). Serialized with all
    /// other writes; skipped when a newer snapshot is already on disk. A
    /// failure forgets the submission so identical bytes are retried.
    pub fn write(&self, path: &Path, ticket: u64, bytes: &[u8]) -> std::io::Result<WriteOutcome> {
        let mut disk = locked(&self.io);
        if ticket < disk.ticket {
            return Ok(WriteOutcome::Skipped);
        }
        match write_atomic(path, bytes) {
            Ok(()) => {
                disk.ticket = ticket;
                disk.bytes = Some(bytes.to_vec());
                Ok(WriteOutcome::Written)
            }
            Err(error) => {
                drop(disk);
                let mut submitted = locked(&self.submitted);
                if submitted.ticket == ticket {
                    submitted.bytes = None;
                }
                Err(error)
            }
        }
    }

    /// Quit-time write: blocks behind any in-flight write, then writes
    /// synchronously unless the file already holds exactly these bytes. Takes
    /// a ticket newer than every queued write, so those become no-ops.
    pub fn write_now(&self, path: &Path, bytes: &[u8]) -> std::io::Result<WriteOutcome> {
        let ticket = self.next_ticket.fetch_add(1, Ordering::SeqCst) + 1;
        {
            let mut submitted = locked(&self.submitted);
            submitted.ticket = ticket;
            submitted.bytes = Some(bytes.to_vec());
        }
        let mut disk = locked(&self.io);
        if disk.bytes.as_deref() == Some(bytes) {
            disk.ticket = disk.ticket.max(ticket);
            return Ok(WriteOutcome::Skipped);
        }
        drop(disk);
        self.write(path, ticket, bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("tablex-gpui-session-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample() -> GpuiSession {
        GpuiSession {
            tabs: vec![
                SessionTab {
                    kind: "query".into(),
                    title: "Report".into(),
                    connection_id: "c1".into(),
                    database: "app".into(),
                    is_pinned: true,
                    sql: "SELECT 1".into(),
                    output_limit: 50,
                    ..Default::default()
                },
                SessionTab {
                    kind: "query".into(),
                    title: "orders".into(),
                    collection: "orders".into(),
                    query_mode: Some(QueryMode::SqlTable),
                    table_where: "id > 3".into(),
                    table_order_by: "id DESC".into(),
                    ..Default::default()
                },
            ],
            active: Some(1),
            collapsed_groups: vec![SessionGroup {
                connection_id: "c1".into(),
                database: String::new(),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn concurrent_atomic_writes_never_corrupt_or_mix_the_file() {
        let dir = temp_dir();
        let path = dir.join(FILE_NAME);
        let payloads: Vec<Vec<u8>> = (0..8u8).map(|n| vec![b'a' + n; 256 * 1024]).collect();
        std::thread::scope(|scope| {
            for payload in &payloads {
                let path = &path;
                scope.spawn(move || {
                    for _ in 0..25 {
                        write_atomic(path, payload).expect("concurrent write succeeds");
                    }
                });
            }
        });
        let contents = std::fs::read(&path).unwrap();
        assert!(
            payloads.contains(&contents),
            "file is exactly one writer's payload, never a mix"
        );
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "no temp files left behind: {leftovers:?}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_late_background_write_never_overwrites_the_quit_time_write() {
        let dir = temp_dir();
        let path = dir.join(FILE_NAME);
        let writer = SessionWriter::default();
        let older = writer.submit(b"older").unwrap();
        // Quit-time write of a newer snapshot lands first...
        assert_eq!(writer.write_now(&path, b"newer").unwrap(), WriteOutcome::Written);
        // ...then the queued older write runs and is skipped.
        assert_eq!(writer.write(&path, older, b"older").unwrap(), WriteOutcome::Skipped);
        assert_eq!(std::fs::read(&path).unwrap(), b"newer");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn quit_write_happens_even_when_identical_bytes_are_still_in_flight() {
        let dir = temp_dir();
        let path = dir.join(FILE_NAME);
        let writer = SessionWriter::default();
        let ticket = writer.submit(b"state").unwrap();
        assert!(writer.submit(b"state").is_none(), "debounce path dedups");
        assert!(!path.exists());
        assert_eq!(writer.write_now(&path, b"state").unwrap(), WriteOutcome::Written);
        assert_eq!(std::fs::read(&path).unwrap(), b"state");
        // The in-flight write is now a no-op and a second quit write is too.
        assert_eq!(writer.write(&path, ticket, b"state").unwrap(), WriteOutcome::Skipped);
        assert_eq!(writer.write_now(&path, b"state").unwrap(), WriteOutcome::Skipped);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_failed_write_is_not_remembered_as_saved() {
        let dir = temp_dir();
        // The "directory" of the target is a regular file: every write fails.
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, "x").unwrap();
        let path = blocker.join(FILE_NAME);
        let writer = SessionWriter::default();
        let ticket = writer.submit(b"state").unwrap();
        assert!(writer.write(&path, ticket, b"state").is_err());
        assert!(
            writer.submit(b"state").is_some(),
            "identical bytes are retried after a failure"
        );
        // Quit-time write also still tries (and reports the failure).
        assert!(writer.write_now(&path, b"state").is_err());
        // Once the target is writable the very same bytes land.
        let good = dir.join("good").join(FILE_NAME);
        assert_eq!(writer.write_now(&good, b"state").unwrap(), WriteOutcome::Written);
        assert_eq!(std::fs::read(&good).unwrap(), b"state");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn concurrent_background_and_quit_writes_leave_one_complete_snapshot() {
        let dir = temp_dir();
        let path = dir.join(FILE_NAME);
        let writer = std::sync::Arc::new(SessionWriter::default());
        let snapshots: Vec<Vec<u8>> = (0..6u8).map(|n| vec![b'a' + n; 128 * 1024]).collect();
        std::thread::scope(|scope| {
            for snapshot in &snapshots {
                let (writer, path) = (writer.clone(), &path);
                scope.spawn(move || {
                    if let Some(ticket) = writer.submit(snapshot) {
                        writer.write(path, ticket, snapshot).unwrap();
                    }
                });
            }
        });
        let latest = vec![b'z'; 128 * 1024];
        writer.write_now(&path, &latest).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), latest, "quit snapshot wins");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn round_trips_through_an_atomic_write() {
        let dir = temp_dir();
        let path = dir.join("nested").join(FILE_NAME);
        let session = sample();
        write_atomic(&path, &encode(&session).unwrap()).unwrap();
        match load(&path) {
            LoadOutcome::Loaded(loaded) => assert_eq!(loaded, session),
            other => panic!("expected Loaded, got {other:?}"),
        }
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            leftovers,
            vec![FILE_NAME.to_string()],
            "no temp file left behind"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_corrupt_and_newer_files_are_distinguished() {
        let dir = temp_dir();
        let path = dir.join(FILE_NAME);
        assert!(matches!(load(&path), LoadOutcome::Missing));
        std::fs::write(&path, "{ not json").unwrap();
        assert!(matches!(load(&path), LoadOutcome::Unusable(_)));
        std::fs::write(&path, r#"{"version": 99, "tabs": []}"#).unwrap();
        assert!(matches!(load(&path), LoadOutcome::Unusable(message) if message.contains("newer")));
        quarantine(&path);
        assert!(!path.exists());
        assert!(dir.join(format!("{FILE_NAME}.corrupt")).exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unknown_and_missing_fields_degrade_gracefully() {
        let dir = temp_dir();
        let path = dir.join(FILE_NAME);
        std::fs::write(
            &path,
            r#"{"tabs":[{"title":"A","future_field":1}],"active":0,"extra":{}}"#,
        )
        .unwrap();
        let LoadOutcome::Loaded(session) = load(&path) else {
            panic!("tolerant load");
        };
        assert_eq!(session.version, SESSION_VERSION);
        assert_eq!(session.tabs[0].title, "A");
        assert_eq!(session.tabs[0].kind, "query");
        assert_eq!(
            session.tabs[0].output_limit,
            crate::query::DEFAULT_OUTPUT_LIMIT
        );
        // Files written before the filter builder existed have no rule fields.
        assert!(session.tabs[0].table_filter_rules.is_empty());
        assert_eq!(session.tabs[0].table_filter_where, None);
        assert!(session.tabs[0].table_hidden_columns.is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn filter_builder_rules_round_trip_and_tolerate_partial_rules() {
        let dir = temp_dir();
        let path = dir.join(FILE_NAME);
        // A hand-trimmed / older rule: only some fields, an unknown extra one.
        std::fs::write(
            &path,
            r#"{"tabs":[{"title":"orders","query_mode":"sqlTable",
                "table_filter_rules":[{"id":4,"column":"id","mode":"between","value":"1","end_value":"9","future":true},
                                      {"column":"name","conjunction":"or","disabled":true}],
                "table_filter_where":"\"id\" BETWEEN 1 AND 9",
                "table_hidden_columns":["name","payload"]}]}"#,
        )
        .unwrap();
        let LoadOutcome::Loaded(session) = load(&path) else {
            panic!("tolerant load");
        };
        let rules = &session.tabs[0].table_filter_rules;
        assert_eq!(session.tabs[0].table_hidden_columns, ["name", "payload"]);
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].mode, crate::sql_filter::SqlFilterMode::Between);
        assert_eq!(rules[0].end_value, "9");
        assert_eq!(rules[1].mode, crate::sql_filter::SqlFilterMode::Equals);
        assert!(rules[1].disabled);
        assert_eq!(rules[1].conjunction, crate::sql_filter::SqlFilterConjunction::Or);
        write_atomic(&path, &encode(&session).unwrap()).unwrap();
        let LoadOutcome::Loaded(again) = load(&path) else {
            panic!("reload");
        };
        assert_eq!(again, session);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn persisted_query_mode_strings_are_unchanged() {
        let modes = [
            "mongoFind",
            "mongoScript",
            "mongoAggregate",
            "sqlTable",
            "postgresTable",
            "sqliteTable",
            "someFutureMode",
        ];
        let tabs = modes
            .iter()
            .map(|mode| format!(r#"{{"title":"t","query_mode":"{mode}"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        let dir = temp_dir();
        let path = dir.join(FILE_NAME);
        std::fs::write(&path, format!(r#"{{"tabs":[{tabs}]}}"#)).unwrap();
        let LoadOutcome::Loaded(session) = load(&path) else {
            panic!("load");
        };
        for (tab, expected) in session.tabs.iter().zip(modes) {
            assert_eq!(tab.query_mode.as_ref().map(QueryMode::as_str), Some(expected));
        }
        assert_eq!(session.tabs[6].query_mode, Some(QueryMode::Unknown("someFutureMode".into())));
        // Saved JSON carries exactly the same strings.
        let saved: serde_json::Value = serde_json::from_slice(&encode(&session).unwrap()).unwrap();
        for (tab, expected) in saved["tabs"].as_array().unwrap().iter().zip(modes) {
            assert_eq!(tab["query_mode"], expected);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn default_path_is_the_gpui_file_never_the_swift_session() {
        let path = default_path().expect("a data dir exists");
        assert!(path.ends_with("tablex/gpui-workspace-session.json"));
    }
}
