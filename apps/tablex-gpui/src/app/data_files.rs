//! Data files opened as tables: CSV, TSV, JSON, NDJSON and Parquet files read through the DuckDB
//! driver (File > Open Data File, the command palette, or a file dropped on the navigator or a SQL
//! editor).
//!
//! One connection, "Files", holds them all: an in-memory DuckDB (`duckdb://:memory:`) with one view
//! per file, `CREATE OR REPLACE VIEW "orders" AS SELECT * FROM read_csv_auto('/path/orders.csv')`.
//! One instance means a query in an editor tab can join two files, and the sidebar grows by one
//! connection however many files are opened. The view reads the file when it is queried (DuckDB
//! streams it, nothing is loaded into the app), so the grid pages, sorts and filters it like any
//! table, and a reopened file shows what the file holds now.
//!
//! The connection is a saved connection like any other while the app runs (rename, colour, delete,
//! the connection menu all work), but a restart must not show a connection whose views are gone and
//! tabs on files that moved: the record is removed at the next start ([`forget_stale_record`]), and
//! the session restore drops the tabs of a connection that no longer exists.
//!
//! The connection goes through the normal connect path, so a DuckDB driver that is not installed
//! opens the missing-driver dialog (Install, Open Driver Manager) and the files open once it is.
//! Agents never reach these views: `tablex-mcp` opens its own DuckDB instance with external access off
//! (`duckdb://:memory:?external_access=off`), which is a different, empty database, and on a file
//! database a view over a file fails in the engine for it. The assistant shares this instance (the
//! classifier alone holds there: no file function, no file as a table), so the connection is saved with
//! agent access off: what the user opened is the user's until the connection's Overrides say otherwise.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use engine_driver_api::QueryRequest;
use engine_storage::connection::SavedConnectionRecord;
use gpui::{Context, PathPromptOptions, Window};
use gpui_component::WindowExt;
use uuid::Uuid;

use super::*;

/// The Files connection (the same id on every run, so a stale record is found at start).
pub(in crate::app) const FILES_ID: Uuid = Uuid::from_u128(0x7461_626c_6578_4669_6c65_7300_0000_0001);
const FILES_NAME: &str = "Files";
const FILES_URI: &str = "duckdb://:memory:";
/// What DuckDB calls an in-memory instance's database, and its default schema.
const DATABASE: &str = "memory";
const SCHEMA: &str = "main";

const NOT_DATA: &str = "Only CSV, TSV, JSON, NDJSON and Parquet files can be opened as tables.";

/// How DuckDB reads a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    Csv,
    Tsv,
    Json,
    Parquet,
}

impl Format {
    fn of(path: &Path) -> Option<Self> {
        let extension = path.extension()?.to_str()?.to_ascii_lowercase();
        Some(match extension.as_str() {
            "csv" => Self::Csv,
            "tsv" => Self::Tsv,
            // `read_json_auto` tells a JSON array of objects from newline-delimited objects itself.
            "json" | "ndjson" | "jsonl" => Self::Json,
            "parquet" => Self::Parquet,
            _ => return None,
        })
    }

    /// The table function that reads the file named by the SQL string literal `file`.
    fn reader(self, file: &str) -> String {
        match self {
            Self::Csv => format!("read_csv_auto({file})"),
            Self::Tsv => format!("read_csv_auto({file}, delim = '\\t')"),
            Self::Json => format!("read_json_auto({file})"),
            Self::Parquet => format!("read_parquet({file})"),
        }
    }
}

pub(in crate::app) fn is_data_file(path: &Path) -> bool {
    Format::of(path).is_some()
}

fn literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// `CREATE OR REPLACE VIEW "view" AS SELECT * FROM <reader>('path')`: the path is a string literal
/// (quotes doubled; DuckDB reads no escapes in one), the name a quoted identifier.
fn view_sql(view: &str, path: &Path, format: Format) -> String {
    let file = literal(&path.to_string_lossy());
    format!("CREATE OR REPLACE VIEW {} AS SELECT * FROM {}", crate::navigator::quote_identifier(view), format.reader(&file))
}

/// A readable view name from a file name: letters and digits stay, every other run of characters
/// becomes one `_`.
fn view_stem(path: &Path) -> String {
    let stem = path.file_stem().map(|stem| stem.to_string_lossy().into_owned()).unwrap_or_default();
    let mut name = String::new();
    for c in stem.chars() {
        if c.is_alphanumeric() {
            name.push(c);
        } else if !name.ends_with('_') {
            name.push('_');
        }
    }
    let name = name.trim_matches('_');
    match name.chars().next() {
        None => "file".to_owned(),
        Some(first) if first.is_ascii_digit() => format!("file_{name}"),
        Some(_) => name.to_owned(),
    }
}

/// What the app keeps about the files opened this run.
#[derive(Default)]
pub(in crate::app) struct DataFiles {
    /// The view of each file, so opening it again finds its view (and its tab) and two files named
    /// alike get views of their own (`orders`, `orders_2`).
    views: HashMap<PathBuf, String>,
    /// Files waiting for the Files connection to come up.
    pub(in crate::app) waiting: Vec<PathBuf>,
}

impl DataFiles {
    fn view_of(&mut self, path: &Path) -> String {
        if let Some(view) = self.views.get(path) {
            return view.clone();
        }
        let base = view_stem(path);
        let taken = |name: &str| self.views.values().any(|view| view.eq_ignore_ascii_case(name));
        let view = (1..)
            .map(|n| if n == 1 { base.clone() } else { format!("{base}_{n}") })
            .find(|name| !taken(name))
            .expect("an unbounded range has a free name");
        self.views.insert(path.to_path_buf(), view.clone());
        view
    }

    /// The views to create again on the session of `connection` that has just connected: an in-memory
    /// instance starts empty, so after a reconnect (or a driver update that reconnects) every file
    /// opened this run needs its view again. Empty for any other connection.
    pub(in crate::app) fn views_for(&self, connection: &str) -> Vec<(PathBuf, String)> {
        if connection != FILES_ID.to_string() {
            return Vec::new();
        }
        let mut views: Vec<_> = self.views.iter().map(|(path, view)| (path.clone(), view.clone())).collect();
        views.sort_by(|a, b| a.1.cmp(&b.1));
        views
    }
}

/// Blocking: creates `views` on the new `session`, before anything else runs on it. A file that
/// moved or went away fails alone: the tab on it says why when it runs.
pub(in crate::app) fn recreate_views(session: &engine_driver_host::SharedDriverSession, views: &[(PathBuf, String)]) {
    for (path, view) in views {
        let _ = create_view(session, path, view);
    }
}

/// Removes the Files connection of an earlier run from `records` and from the store. Its views died
/// with that run; showing it again would be an empty connection, and tabs on it errors.
pub(in crate::app) fn forget_stale_record(records: &mut Vec<SavedConnectionRecord>) {
    let id = FILES_ID.to_string().to_ascii_uppercase();
    if records.iter().any(|record| record.id.eq_ignore_ascii_case(&id)) {
        records.retain(|record| !record.id.eq_ignore_ascii_case(&id));
        let _ = engine_storage::connection::delete_connection(&id);
    }
}

/// Why `path` cannot be read as a file, in a sentence.
fn unreadable(path: &Path) -> Option<String> {
    let reason = |error: std::io::Error| match error.kind() {
        std::io::ErrorKind::NotFound => "the file does not exist".to_owned(),
        std::io::ErrorKind::PermissionDenied => "permission denied".to_owned(),
        _ => error.to_string(),
    };
    // Metadata first: opening a named pipe would wait for a writer.
    match std::fs::metadata(path) {
        Err(error) => Some(reason(error)),
        Ok(metadata) if !metadata.is_file() => Some("it is not a file".to_owned()),
        Ok(_) => std::fs::File::open(path).err().map(reason),
    }
}

/// Blocking: creates the view of `path` on `session`. Never on the UI thread.
fn create_view(session: &engine_driver_host::SharedDriverSession, path: &Path, view: &str) -> Result<(), String> {
    if let Some(reason) = unreadable(path) {
        return Err(reason);
    }
    let format = Format::of(path).ok_or_else(|| NOT_DATA.to_owned())?;
    let failed = |error: &dyn std::fmt::Display| super::data_import::message_of(&crate::sessions::redact_secrets(&error.to_string()));
    let runner = session.query_runner().map_err(|error| failed(&error))?;
    let request = serde_json::to_string(&QueryRequest {
        kind: "sql".into(),
        operation_id: runner.operation_id(),
        database: None,
        object: None,
        offset: 0,
        limit: 1,
        payload: serde_json::json!({"sql": view_sql(view, path, format), "confirmed": true}),
    })
    .map_err(|error| error.to_string())?;
    let page = runner.open_query(&request).map_err(|error| failed(&error))?;
    runner.release_page(page.page_id);
    Ok(())
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |name| name.to_string_lossy().into_owned())
}

impl AppRoot {
    /// File > Open Data File: asks for the files and opens them as tables.
    pub(super) fn open_data_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // gpui's path prompt takes no extension filter: what is picked is checked below.
        let prompt = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: true, prompt: Some("Open data file".into()) });
        cx.spawn_in(window, async move |this, cx| {
            let paths = match prompt.await {
                Ok(Ok(Some(paths))) => paths,
                Ok(Ok(None)) => return,
                _ => {
                    let _ = this.update_in(cx, |_, window, cx| window.push_notification(t!("editor.could_not_open_the_file_picker"), cx));
                    return;
                }
            };
            let _ = this.update_in(cx, |this, window, cx| this.open_data_files(paths, window, cx));
        })
        .detach();
    }

    /// Opens the data files of `paths` as tables on the Files connection; the others are named in a
    /// notification and skipped.
    pub(super) fn open_data_files(&mut self, paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        let (files, skipped): (Vec<_>, Vec<_>) = paths.into_iter().partition(|path| is_data_file(path));
        if files.is_empty() {
            window.push_notification(NOT_DATA, cx);
            return;
        }
        if !skipped.is_empty() {
            window.push_notification(format!("{} skipped: {}", skipped.iter().map(|path| file_name(path)).collect::<Vec<_>>().join(", "), NOT_DATA), cx);
        }
        // A file reached by a symlink or a relative path is the file it points to: one view, one tab.
        // Resolved off the UI thread (a network volume can be slow); a path that cannot be keeps its name
        // and fails with the reason when it is read.
        cx.spawn_in(window, async move |this, cx| {
            let files = cx
                .background_executor()
                .spawn(async move {
                    smol::unblock(move || files.into_iter().map(|path| std::fs::canonicalize(&path).unwrap_or(path)).collect::<Vec<_>>()).await
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| this.open_resolved_data_files(files, window, cx));
        })
        .detach();
    }

    fn open_resolved_data_files(&mut self, files: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        let key = match self.ensure_files_connection(cx) {
            Ok(key) => key,
            Err(error) => {
                self.report_error(Area::Connect, None, &error);
                window.push_notification(error, cx);
                return;
            }
        };
        if self.sessions.is_connected(&key) && self.driver_runtime.sessions.contains_key(&key) {
            self.create_data_views(&key, files, cx);
        } else {
            // The connect may stop at the missing-driver dialog; the files open when it succeeds.
            self.data_files.waiting.extend(files);
            self.ensure_connected(key, None, cx);
        }
    }

    /// The Files connection, saved first when this run has none yet.
    fn ensure_files_connection(&mut self, cx: &mut Context<Self>) -> Result<String, String> {
        let key = FILES_ID.to_string();
        if self.saved_connections.iter().any(|saved| saved.id == FILES_ID) {
            return Ok(key);
        }
        let now = connection_ui::current_time();
        let record = SavedConnectionRecord {
            id: key.to_ascii_uppercase(),
            name: FILES_NAME.to_owned(),
            uri: FILES_URI.to_owned(),
            db_type: crate::connections::DB_TYPE_DUCKDB.to_owned(),
            color_hex: "#34C759".to_owned(),
            save_password: false,
            save_proxy_password: false,
            network_settings_json: "{}".to_owned(),
            is_pinned: false,
            last_connected: None,
            created_at: now,
            updated_at: now,
            connection_type: String::new(),
            // The user's own files are not for an AI agent unless the connection's Overrides say so.
            overrides_json: format!("{{\"{}\":\"{}\"}}", engine_connect::profile::AGENT_ACCESS, engine_connect::profile::AgentLevel::Off.id()),
        };
        engine_storage::connection::upsert_connection(&record).map_err(|error| format!("Could not add the Files connection: {error}"))?;
        if self.refresh_saved_connections(cx) {
            Ok(key)
        } else {
            Err("Could not add the Files connection: the saved connections could not be read".to_owned())
        }
    }

    /// The Files connection came up: the files that waited for it open now.
    pub(super) fn connected_for_data_files(&mut self, connection: &str, cx: &mut Context<Self>) {
        if connection != FILES_ID.to_string() || self.data_files.waiting.is_empty() {
            return;
        }
        let files = std::mem::take(&mut self.data_files.waiting);
        self.create_data_views(connection, files, cx);
    }

    /// The Files connection did not come up (no driver, a cancelled install): nothing opens.
    pub(super) fn data_files_connect_failed(&mut self, connection: &str, message: &str, cx: &mut Context<Self>) {
        if connection != FILES_ID.to_string() || self.data_files.waiting.is_empty() {
            return;
        }
        let count = std::mem::take(&mut self.data_files.waiting).len();
        let text = format!("Could not open {} data file{}: {message}", count, if count == 1 { "" } else { "s" });
        self.with_ui_window(cx, move |window, app| window.push_notification(text, app));
    }

    /// Creates a view per file off the UI thread, then opens a table tab on each that worked.
    fn create_data_views(&mut self, connection: &str, files: Vec<PathBuf>, cx: &mut Context<Self>) {
        let Some(session) = self.driver_runtime.sessions.get(connection).cloned() else {
            return;
        };
        let jobs: Vec<(PathBuf, String)> = files
            .into_iter()
            .map(|path| {
                let view = self.data_files.view_of(&path);
                (path, view)
            })
            .collect();
        let connection = connection.to_owned();
        cx.spawn(async move |this, cx| {
            let done = cx
                .background_executor()
                .spawn(async move {
                    smol::unblock(move || {
                        jobs.into_iter().map(|(path, view)| (create_view(&session, &path, &view), path, view)).collect::<Vec<_>>()
                    })
                    .await
                })
                .await;
            let _ = this.update(cx, |this, cx| this.data_views_created(&connection, done, cx));
        })
        .detach();
    }

    fn data_views_created(&mut self, connection: &str, done: Vec<(Result<(), String>, PathBuf, String)>, cx: &mut Context<Self>) {
        let mut created = Vec::new();
        for (outcome, path, view) in done {
            match outcome {
                Ok(()) => created.push(view),
                Err(error) => {
                    // Not a view that exists: a reconnect has nothing to create again for it.
                    self.data_files.views.remove(&path);
                    let text = format!("Could not open {}: {error}", file_name(&path));
                    self.report_error(Area::Query, Some(connection), format!("{text} ({})", path.display()));
                    self.with_ui_window(cx, move |window, app| window.push_notification(text, app));
                }
            }
        }
        if created.is_empty() {
            return;
        }
        // The new views show in the sidebar tree, and the last file opened is the active tab.
        self.refresh_connection_id(connection, cx);
        for view in created {
            let (tab, _) = self.open_sql_table_tab(&view, connection, DATABASE, SCHEMA, &view);
            self.load_table_structure(tab, connection, DATABASE.to_owned(), SCHEMA.to_owned(), view, cx);
            // A file opened again is read again: it may have changed since.
            self.run_active_tab_query(cx);
        }
        self.notify_chrome(cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_extension_decides_how_a_file_is_read_whatever_its_case() {
        for (name, format) in [("a.csv", Format::Csv), ("a.TSV", Format::Tsv), ("a.json", Format::Json), ("a.ndjson", Format::Json), ("a.JSONL", Format::Json), ("a.Parquet", Format::Parquet)] {
            assert_eq!(Format::of(Path::new(name)), Some(format), "{name}");
        }
        for name in ["a.txt", "a", "a.csv.gz", "csv", ".csv.bak", "a.sql"] {
            assert_eq!(Format::of(Path::new(name)), None, "{name}");
        }
    }

    #[test]
    fn the_path_is_a_string_literal_and_the_view_a_quoted_name() {
        let sql = view_sql("my \"view\"", Path::new("/tmp/it's here/dữ liệu.csv"), Format::Csv);
        assert_eq!(sql, "CREATE OR REPLACE VIEW \"my \"\"view\"\"\" AS SELECT * FROM read_csv_auto('/tmp/it''s here/dữ liệu.csv')");
        assert_eq!(view_sql("t", Path::new("/a.tsv"), Format::Tsv), "CREATE OR REPLACE VIEW \"t\" AS SELECT * FROM read_csv_auto('/a.tsv', delim = '\\t')");
        assert!(view_sql("t", Path::new("/a.json"), Format::Json).ends_with("read_json_auto('/a.json')"));
        assert!(view_sql("t", Path::new("/a.parquet"), Format::Parquet).ends_with("read_parquet('/a.parquet')"));
    }

    #[test]
    fn views_are_named_after_the_file_and_two_files_alike_get_two_views() {
        assert_eq!(view_stem(Path::new("/x/Sales Q1 (final).csv")), "Sales_Q1_final");
        assert_eq!(view_stem(Path::new("/x/2024-orders.csv")), "file_2024_orders");
        assert_eq!(view_stem(Path::new("/x/dữ liệu.json")), "dữ_liệu");
        assert_eq!(view_stem(Path::new("/x/---.csv")), "file");
        let mut files = DataFiles::default();
        let (a, b, c) = (Path::new("/a/orders.csv"), Path::new("/b/Orders.parquet"), Path::new("/c/orders.tsv"));
        assert_eq!(files.view_of(a), "orders");
        assert_eq!(files.view_of(b), "Orders_2", "DuckDB names do not differ by case");
        assert_eq!(files.view_of(c), "orders_3");
        assert_eq!(files.view_of(a), "orders", "the same file, the same view");
    }
}
