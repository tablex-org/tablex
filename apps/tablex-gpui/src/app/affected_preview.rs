//! What an `UPDATE` / `DELETE` will change, shown inside the confirmation that asks before it runs.
//!
//! The statement's table and `WHERE` are read once, as a stream of lexer tokens
//! (`crate::affected_rows`), and become two reads: `SELECT count(*) ...` and `SELECT * ... LIMIT n`.
//! They run one after the other on the tab's driver session with `payload.read_only`, so the server
//! itself refuses a write: the preview never writes, whatever the text of the statement is. A
//! `WHERE` that calls a function with effects a rollback does not undo (a lock, a sequence) is not
//! copied into a read at all ([`Shape::SideEffects`]).
//!
//! On a tab's own session the user's transaction may be open, and a read there must leave it as it
//! was, also when it is cancelled or fails. [`InTransaction`] says per connection kind where that
//! holds: PostgreSQL (a savepoint, from driver 0.1.5), SQLite (stopping a `SELECT` rolls nothing
//! back) and MySQL / MariaDB (the driver refuses to read there). DuckDB aborts the transaction on any
//! error or interrupt, so while one is open its statements are not previewed.
//!
//! The dialog does not wait for them: it opens at once with "Counting...", and the count and the
//! rows land in it when they are read. Running never waits for the preview either. The query in
//! flight is cancelled when the dialog goes away (Run, Cancel, Escape), because the preview entity
//! the dialog holds is released with it, and when the reads take longer than [`READ_DEADLINE`].
//! When Run is clicked in the first dialog, what it read goes to the one dialog that may follow in
//! the same run (the driver's "dangerous statement" one, or the script's): it shows those rows
//! instead of reading them twice. That hand-off is used once and dropped by anything else: another
//! run, the run executing, a change of the tab's connection or database. Nothing is ever kept
//! past that, so a count is never shown for data it was not read from.
//!
//! Nothing here runs unless a confirmation is shown for an `UPDATE` / `DELETE`: no work on the
//! run path of a `SELECT`, nor of a statement nobody asks about.

use super::*;
use crate::affected_rows::{self, Kind, Shape, PREVIEW_ROWS};
use crate::grid::{CellPosition, GridSource as _};
use crate::grid_arrow::{ArrowGridSource, BinaryDisplay};
use crate::script_run::one_line;
use gpui::{Entity, SharedString, Task};
use gpui_component::table::{Column, DataTable, TableDelegate, TableState};
use std::collections::HashMap;

/// After this long without the count the dialog says it is still counting, and that Run does not wait.
const SLOW_AFTER: Duration = Duration::from_secs(3);

/// The reads of a preview on a tab's own session are given up after this long (the count of a huge
/// table holds a read-only snapshot for as long as it runs).
const READ_DEADLINE: Duration = Duration::from_secs(20);

/// The same on the connection's shared session, which every other auto-commit tab of it uses too:
/// a count must not hold it for long while the dialog sits open.
const SHARED_READ_DEADLINE: Duration = Duration::from_secs(5);

/// Rows of the preview grid that show without scrolling: its box is this tall from the moment the
/// dialog opens, so the buttons below do not move when the rows land (a click aimed at one must hit it).
const VISIBLE_ROWS: usize = 6;
/// The same for a script, whose sections stack.
const SCRIPT_VISIBLE_ROWS: usize = 3;

/// The most the sections take of the dialog's height before they scroll.
const SECTIONS_MAX_HEIGHT: f32 = 340.0;

/// The grid's row height (`gpui-component` table, default size) the grid's box is sized by.
const ROW_HEIGHT: f32 = 28.0;

/// Longest reason of a failed read the box shows.
const REASON_CHARS: usize = 110;

/// What one section read: the count (when it could be read) and the first rows.
type Read = (Option<u64>, ArrowGridSource);

/// The reads still to make: section, count SQL, rows SQL.
type Reads = Vec<(usize, String, String)>;

/// The sections the cache filled.
type Hits = Vec<(usize, Read)>;

/// The rows a preview read, in the grid's own formatting.
type Landed = Result<Read, DriverQueryError>;

/// What the confirmation the user just answered Run to had read, for the one dialog of the same run
/// that may follow it (see the module's docs). Taken by it, or dropped.
pub(super) struct Handoff {
    tab_id: u64,
    connection: String,
    database: String,
    /// By count query.
    reads: Vec<(String, Read)>,
}

/// Where a dialog stands in a run's chain of confirmations.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Chain {
    /// The first confirmation of a run: reads for itself, and drops any hand-off.
    First,
    /// The one after Run was answered: takes what the first read.
    Continued,
}

/// Whether the previews can be read.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Readiness {
    Ready,
    /// The driver lacks a capability a read-only read needs.
    Unsupported,
    /// The tab's own session needs a newer driver (see [`InTransaction::Reads`]).
    OldDriver,
    /// The user's transaction is open on the tab's session, and a read there would put it at risk.
    InTransaction,
}

/// Whether a preview may read on a session the user's transaction is open on.
enum InTransaction {
    /// It may, and a cancel or a failure leaves the transaction alone; PostgreSQL's driver does so
    /// (in a savepoint) from the version given. MySQL / MariaDB are here too: their driver refuses
    /// the read (`query.read_only_unavailable`), which the dialog reports.
    Reads { since: Option<&'static str> },
    /// A cancel or a failure of the read aborts the transaction, losing the user's uncommitted work.
    Never,
}

impl InTransaction {
    fn of(kind: ConnectionKind) -> Self {
        match kind {
            ConnectionKind::Postgres => Self::Reads { since: Some("0.1.5") },
            // Every error or interrupt invalidates the active transaction.
            ConnectionKind::Duckdb => Self::Never,
            // SQLite: `sqlite3_interrupt` of a `SELECT` rolls nothing back (only a write is). The rest
            // have no transaction a tab can hold.
            _ => Self::Reads { since: None },
        }
    }
}

/// One statement of the confirmation. Every text is made when the section changes, never in a frame.
struct Section {
    /// The statement's start (a script's sections tell them apart); `None` for a lone statement.
    heading: Option<SharedString>,
    kind: Kind,
    table: SharedString,
    whole_table: bool,
    /// Said under the heading: how many rows, or why there is no preview. Empty: nothing to say.
    line: SharedString,
    /// What else the user should know (SQLite's `OR REPLACE` also deletes).
    note: Option<SharedString>,
    /// The statement changes every row.
    warning: Option<SharedString>,
    grid: Option<Entity<TableState<RowsDelegate>>>,
    /// Rows the grid holds.
    rows: usize,
    /// Height of the box the grid lands in; 0 when no rows are coming.
    box_height: f32,
    /// What the box says when it holds no grid: no rows match, or why they could not be read.
    box_note: Option<SharedString>,
    /// Its reads are still to come.
    counting: bool,
    /// No preview is coming for it (not understood, refused by the driver, failed).
    unavailable: bool,
    /// The count query, and what was read: what the next dialog is given.
    read: Option<(String, Read)>,
}

impl Section {
    fn new(heading: Option<SharedString>, preview: &affected_rows::Preview) -> Self {
        let mut section = Self {
            heading,
            kind: preview.kind,
            table: preview.table.clone().into(),
            whole_table: preview.whole_table,
            // A statement without WHERE says it all in its warning: nothing else moves when its count lands.
            line: if preview.whole_table {
                SharedString::default()
            } else {
                t!("editor.affected_counting").into_owned().into()
            },
            note: preview.replaces.then(|| t!("editor.affected_or_replace").into_owned().into()),
            warning: None,
            grid: None,
            rows: 0,
            box_height: 0.0,
            box_note: None,
            counting: true,
            unavailable: false,
            read: None,
        };
        section.warn(None);
        section
    }

    /// A statement the rows of which cannot be read beforehand; `line` says why.
    fn unsupported(heading: Option<SharedString>, line: std::borrow::Cow<'static, str>) -> Self {
        Self {
            heading,
            kind: Kind::Update,
            table: SharedString::default(),
            whole_table: false,
            line: line.into_owned().into(),
            note: None,
            warning: None,
            grid: None,
            rows: 0,
            box_height: 0.0,
            box_note: None,
            counting: false,
            unavailable: true,
            read: None,
        }
    }

    /// The warning of a statement without `WHERE`, with the row count once it is known.
    fn warn(&mut self, count: Option<u64>) {
        if !self.whole_table {
            return;
        }
        let table = self.table.to_string();
        self.warning = Some(
            match count {
                Some(1) => t!("editor.affected_whole_table_one", table = table),
                Some(count) => t!("editor.affected_whole_table_other", table = table, count = count),
                None => t!("editor.affected_whole_table", table = table),
            }
            .into_owned()
            .into(),
        );
    }

    /// No preview comes for it: `line` says so, and the box (which keeps its height, so nothing
    /// moves) gives the reason on one line. A section with nothing above its box (a statement
    /// without WHERE) says it in the box.
    fn fail(&mut self, line: SharedString, reason: Option<SharedString>) {
        if self.box_height > 0.0 && self.line.is_empty() {
            self.box_note = reason.or(Some(line));
        } else {
            self.line = line;
            self.box_note = reason;
        }
        (self.counting, self.unavailable) = (false, true);
    }
}

/// The confirmation's preview: one section per `UPDATE` / `DELETE` of the statement or script.
pub(super) struct AffectedPreview {
    sections: Vec<Section>,
    /// "N more UPDATE or DELETE statements are not previewed", for a script past the ones shown.
    more: Option<SharedString>,
    /// "Counted before the script runs ...", for more than one statement.
    script_note: Option<SharedString>,
    /// The hint of a count that takes long.
    slow_hint: SharedString,
    /// Counting has taken [`SLOW_AFTER`] and is not done.
    slow: bool,
    /// Some section has reads to come (so the hint's row is kept).
    reads: bool,
    /// The read in flight, cancelled when this is released or the reads time out.
    operation: Option<DriverQueryOperation>,
    /// Counts the cancelled reads for the tests.
    #[cfg(test)]
    root: gpui::WeakEntity<AppRoot>,
    _reads: Option<Task<()>>,
    _clock: Option<Task<()>>,
}

impl AppRoot {
    /// The preview a confirmation of `sql` (run on `tab_id`) shows, started; `None` when it has
    /// none to show: the setting is off, the tab is not a SQL editor, or the text changes no data.
    pub(super) fn affected_preview_for(
        &mut self,
        tab_id: u64,
        sql: &str,
        chain: Chain,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<AffectedPreview>> {
        self.affected_preview_with(tab_id, chain, window, cx, |lex, dialect| affected_rows::scan(sql, lex, dialect))
    }

    /// [`Self::affected_preview_for`] for the statements of a script, which are never joined.
    pub(super) fn affected_preview_for_statements<'a>(
        &mut self,
        tab_id: u64,
        statements: impl Iterator<Item = &'a str>,
        chain: Chain,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<AffectedPreview>> {
        self.affected_preview_with(tab_id, chain, window, cx, |lex, dialect| {
            affected_rows::scan_statements(statements, lex, dialect)
        })
    }

    /// Run was answered in the confirmation showing `preview`: what it read goes to the next dialog
    /// of this run, if there is one. Called just before the run starts.
    pub(super) fn hand_off_preview(&mut self, tab_id: u64, preview: Option<&Entity<AffectedPreview>>, cx: &gpui::App) {
        self.driver_runtime.preview_handoff = None;
        let Some(preview) = preview else {
            return;
        };
        let reads = preview.read(cx).landed_reads();
        let Some(tab) = self.workspace.tab(tab_id).filter(|_| !reads.is_empty()) else {
            return;
        };
        self.driver_runtime.preview_handoff =
            Some(Handoff { tab_id, connection: tab.connection_id.clone(), database: tab.database.clone(), reads });
    }

    /// The run goes on without asking again, or another one starts: nothing is handed on.
    pub(super) fn drop_preview_handoff(&mut self) {
        self.driver_runtime.preview_handoff = None;
    }

    /// What the hand-off holds for `tab_id`, once: nothing when it is another tab's, or when the tab's
    /// connection or database is not the one it was read on.
    fn take_preview_handoff(&mut self, tab_id: u64) -> HashMap<String, Read> {
        let Some(handoff) = self.driver_runtime.preview_handoff.take() else {
            return HashMap::new();
        };
        let same = self.workspace.tab(tab_id).is_some_and(|tab| {
            handoff.tab_id == tab_id && handoff.connection == tab.connection_id && handoff.database == tab.database
        });
        if same {
            handoff.reads.into_iter().collect()
        } else {
            HashMap::new()
        }
    }

    fn affected_preview_with(
        &mut self,
        tab_id: u64,
        chain: Chain,
        window: &mut Window,
        cx: &mut Context<Self>,
        scan: impl FnOnce(&engine_driver_kit::sql::SqlLexOptions, engine_plan::ExplainDialect) -> affected_rows::Scan,
    ) -> Option<Entity<AffectedPreview>> {
        #[cfg(all(test, feature = "bench"))]
        if let Some(fixture) = self.driver_runtime.preview_fixture.take() {
            return Some(fixture);
        }
        let handed = match chain {
            Chain::First => {
                self.driver_runtime.preview_handoff = None;
                HashMap::new()
            }
            Chain::Continued => self.take_preview_handoff(tab_id),
        };
        if !self.settings.editor.preview_affected_rows {
            return None;
        }
        let dialect = self.explain_dialect_of(tab_id)?;
        let scan = scan(&self.sql_lex(tab_id), dialect);
        if scan.found.is_empty() {
            return None;
        }
        let tab = self.workspace.tab(tab_id)?;
        let connection = tab.connection_id.clone();
        let database = (!tab.database.is_empty()).then(|| tab.database.clone());
        let saved = self.saved_connections.iter().find(|saved| saved.matches_id(&connection));
        let readiness = match saved.and_then(|saved| self.driver_capabilities(&connection, saved.kind.driver_id())) {
            Some(capabilities)
                if {
                    use engine_driver_api::Capability;
                    capabilities.has(Capability::QuerySql)
                        && capabilities.has(Capability::QueryArrowCData)
                        && capabilities.has(Capability::QueryReadOnly)
                } =>
            {
                saved.map_or(Readiness::Ready, |saved| self.readiness_on_own_session(tab_id, &connection, saved.kind))
            }
            _ => Readiness::Unsupported,
        };
        let root = cx.entity().downgrade();
        let deadline = if self.own_session(tab_id, &connection).is_some() { READ_DEADLINE } else { SHARED_READ_DEADLINE };
        let preview = cx.new(|cx| {
            AffectedPreview::start(root, Target { tab_id, connection, database, deadline }, scan, readiness, handed, window, cx)
        });
        #[cfg(test)]
        {
            self.driver_runtime.last_preview = Some(preview.downgrade());
        }
        Some(preview)
    }

    /// Whether a preview of `tab_id` may read on the session it would use: always on the connection's
    /// shared session; on the tab's own one as [`InTransaction`] says (a driver whose version is not
    /// known is not judged).
    fn readiness_on_own_session(&self, tab_id: u64, connection: &str, kind: ConnectionKind) -> Readiness {
        use super::updates::check::Version;
        if self.own_session(tab_id, connection).is_none() {
            return Readiness::Ready;
        }
        match InTransaction::of(kind) {
            InTransaction::Never if self.tab_tx(tab_id).is_some_and(|tx| tx.is_open()) => Readiness::InTransaction,
            InTransaction::Reads { since: Some(minimum) } => {
                let installed = self.driver_ui.records.get(kind.driver_id()).and_then(|record| Version::parse(&record.version));
                match (installed, Version::parse(minimum)) {
                    (Some(installed), Some(minimum)) if installed < minimum => Readiness::OldDriver,
                    _ => Readiness::Ready,
                }
            }
            _ => Readiness::Ready,
        }
    }

    /// A read-only query of the preview, on the tab's own session when it has one (it sees what the
    /// statement will see), else the connection's.
    pub(super) fn begin_preview_query(&self, tab_id: u64, connection: &str) -> Result<PreparedDriverQuery, DriverQueryError> {
        let own = self.own_session(tab_id, connection).is_some().then_some(tab_id);
        self.begin_tab_driver_query(own, connection)
    }
}

/// Where the preview's reads go.
#[derive(Clone)]
struct Target {
    tab_id: u64,
    connection: String,
    database: Option<String>,
    /// How long the reads may take: shorter on the connection's shared session.
    deadline: Duration,
}

impl AffectedPreview {
    /// The sections of `scan`, and the reads (section, count SQL, rows SQL) that fill the ones that
    /// can be. A section whose statement `cached` holds is filled by it.
    fn new(
        scan: affected_rows::Scan,
        readiness: Readiness,
        root: gpui::WeakEntity<AppRoot>,
        cached: &HashMap<String, Read>,
    ) -> (Self, Reads, Hits) {
        #[cfg(not(test))]
        let _ = root;
        let script = scan.found.len() + scan.more > 1;
        let mut sections = Vec::with_capacity(scan.found.len());
        let (mut reads, mut hits) = (Vec::new(), Vec::new());
        for found in scan.found {
            let heading = script.then(|| SharedString::from(found.heading));
            match found.shape {
                Shape::Preview(preview) if readiness == Readiness::Ready => {
                    let mut section = Section::new(heading, &preview);
                    let rows = if script { SCRIPT_VISIBLE_ROWS } else { VISIBLE_ROWS };
                    section.box_height = ROW_HEIGHT * (rows + 1) as f32 + 2.0;
                    match cached.get(&preview.count_sql) {
                        Some(read) => hits.push((sections.len(), read.clone())),
                        None => reads.push((sections.len(), preview.count_sql, preview.rows_sql)),
                    }
                    sections.push(section);
                }
                Shape::Preview(preview) => {
                    let mut section = Section::new(heading, &preview);
                    // No box is coming: the text goes where the line is.
                    section.line = match readiness {
                        Readiness::OldDriver => t!("editor.affected_old_pg_driver"),
                        Readiness::InTransaction => t!("editor.affected_in_transaction"),
                        _ => t!("editor.affected_unavailable"),
                    }
                    .into_owned()
                    .into();
                    (section.counting, section.unavailable) = (false, true);
                    sections.push(section);
                }
                Shape::Unsupported => sections.push(Section::unsupported(heading, t!("editor.affected_unavailable"))),
            Shape::SideEffects => sections.push(Section::unsupported(heading, t!("editor.affected_side_effects"))),
            }
        }
        let preview = Self {
            sections,
            more: (scan.more > 0).then(|| {
                match scan.more {
                    1 => t!("editor.affected_more_one"),
                    count => t!("editor.affected_more_other", count = count),
                }
                .into_owned()
                .into()
            }),
            script_note: script.then(|| t!("editor.affected_script_note").into_owned().into()),
            slow_hint: t!("editor.affected_counting_slow").into_owned().into(),
            slow: false,
            reads: !reads.is_empty(),
            operation: None,
            #[cfg(test)]
            root,
            _reads: None,
            _clock: None,
        };
        (preview, reads, hits)
    }

    #[allow(clippy::too_many_arguments)]
    fn start(
        root: gpui::WeakEntity<AppRoot>,
        target: Target,
        scan: affected_rows::Scan,
        readiness: Readiness,
        cached: HashMap<String, Read>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (mut preview, reads, hits) = Self::new(scan, readiness, root.clone(), &cached);
        for (index, read) in hits {
            preview.landed(index, Ok(read), window, cx);
        }
        if reads.is_empty() {
            return preview;
        }
        // Released with the dialog: whatever the preview still has in flight stops.
        cx.on_release(move |this: &mut Self, cx| {
            if let Some(operation) = this.operation.take() {
                this.note_cancelled(cx);
                cancel_driver_operation(operation, cx);
            }
        })
        .detach();
        // The hint, then the end of the wait.
        let deadline = target.deadline;
        preview._clock = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SLOW_AFTER).await;
            let _ = this.update(cx, |this, cx| {
                this.slow = true;
                cx.notify();
            });
            cx.background_executor().timer(deadline.saturating_sub(SLOW_AFTER)).await;
            let _ = this.update(cx, |this, cx| this.expire(cx));
        }));
        let executor = cx.background_executor().clone();
        preview._reads = Some(cx.spawn_in(window, async move |this, cx| {
            for (index, count_sql, rows_sql) in reads {
                let landed = read(&this, &root, &target, &executor, count_sql.clone(), rows_sql, cx).await;
                // A cancel the dialog did not ask for (a reconnect, a driver update) ends the sections
                // behind it too: no read after it would be answered either.
                let cancelled = matches!(&landed, Err(error) if error.is_cancelled());
                let landed_in = this.update_in(cx, |this, window, cx| {
                    this.landed_from(index, count_sql, landed, window, cx);
                    if cancelled {
                        this.end_count_cancelled(cx);
                    }
                });
                if landed_in.is_err() || cancelled {
                    return;
                }
            }
        }));
        preview
    }

    /// The reads took too long: the one in flight is cancelled and none starts after it. What was
    /// read before stays.
    fn expire(&mut self, cx: &mut Context<Self>) {
        if !self.sections.iter().any(|section| section.counting) {
            return;
        }
        if let Some(operation) = self.operation.take() {
            self.note_cancelled(cx);
            cancel_driver_operation(operation, cx);
        }
        self._reads = None;
        let line: SharedString = t!("editor.affected_timeout").into_owned().into();
        for section in self.sections.iter_mut().filter(|section| section.counting) {
            section.fail(line.clone(), None);
        }
        cx.notify();
    }

    /// The sections still waiting for a read that will not start fail like the one that was cancelled.
    fn end_count_cancelled(&mut self, cx: &mut Context<Self>) {
        let reason: SharedString = t!("editor.affected_cancelled").into_owned().into();
        let line: SharedString = t!("editor.affected_unavailable").into_owned().into();
        for section in self.sections.iter_mut().filter(|section| section.counting) {
            section.fail(line.clone(), Some(reason.clone()));
        }
        cx.notify();
    }

    /// Counts a cancelled read for the tests.
    fn note_cancelled(&self, cx: &mut gpui::App) {
        #[cfg(test)]
        let _ = self.root.update(cx, |app, _| app.driver_runtime.cancelled_previews += 1);
        #[cfg(not(test))]
        let _ = cx;
    }

    /// A preview whose reads the test feeds by hand (`landed`): no session is needed.
    #[cfg(all(test, feature = "bench"))]
    pub(super) fn fixture(scan: affected_rows::Scan, root: gpui::WeakEntity<AppRoot>) -> Self {
        Self::new(scan, Readiness::Ready, root, &HashMap::new()).0
    }

    /// What the sections have read: the count query and the rows, for the hand-off.
    fn landed_reads(&self) -> Vec<(String, Read)> {
        self.sections.iter().filter_map(|section| section.read.clone()).collect()
    }

    /// A section's reads are done, and what they read is kept.
    fn landed_from(&mut self, index: usize, key: String, landed: Landed, window: &mut Window, cx: &mut Context<Self>) {
        if let Ok(read) = &landed {
            if let Some(section) = self.sections.get_mut(index) {
                section.read = Some((key, read.clone()));
            }
        }
        self.landed(index, landed, window, cx);
    }

    /// A section's reads are done.
    pub(super) fn landed(&mut self, index: usize, landed: Landed, window: &mut Window, cx: &mut Context<Self>) {
        let Some(section) = self.sections.get_mut(index) else {
            return;
        };
        match landed {
            Ok((count, rows)) => {
                section.counting = false;
                section.line = match (count, section.kind) {
                    (Some(1), Kind::Update) => t!("editor.affected_update_one"),
                    (Some(count), Kind::Update) => t!("editor.affected_update_other", count = count),
                    (Some(1), Kind::Delete) => t!("editor.affected_delete_one"),
                    (Some(count), Kind::Delete) => t!("editor.affected_delete_other", count = count),
                    (None, _) => t!("editor.affected_unavailable"),
                }
                .into_owned()
                .into();
                section.warn(count);
                if section.whole_table {
                    section.line = SharedString::default();
                }
                section.rows = rows.row_count();
                if section.rows > 0 {
                    let delegate = RowsDelegate { source: rows };
                    section.grid = Some(cx.new(|cx| {
                        TableState::new(delegate, window, cx)
                            .col_resizable(true)
                            .col_movable(false)
                            .sortable(false)
                            .row_header(false)
                    }));
                } else {
                    section.box_note = Some(t!("editor.affected_no_rows").into_owned().into());
                }
            }
            // The dialog going away never gets here (the preview is released with it): a cancel that
            // does is not the dialog's (a reconnect, a driver update), and no more rows are coming.
            Err(error) => {
                let reason = if error.is_cancelled() {
                    t!("editor.affected_cancelled").into_owned()
                } else if error.is_read_only_unavailable() {
                    // MySQL / MariaDB: a transaction of the user's is open on the session.
                    t!("editor.affected_in_transaction").into_owned()
                } else {
                    one_line(&crate::sessions::redact_secrets(&error.status_text()), REASON_CHARS)
                };
                section.fail(t!("editor.affected_unavailable").into_owned().into(), Some(reason.into()));
            }
        }
        cx.notify();
    }

    /// Cancels the read in flight the way something other than the dialog would (a reconnect, a
    /// driver update): through the driver, leaving the preview to find out (tests).
    #[cfg(test)]
    pub(super) fn cancel_from_outside(&self) {
        if let Some(operation) = &self.operation {
            let _ = operation
                .handle
                .cancel_operation(operation.connection_id, &engine_driver_api::CancelRequest { operation_id: operation.operation_id });
        }
    }

    /// Whether some section is still counting (tests).
    #[cfg(test)]
    pub(super) fn counting(&self) -> bool {
        self.sections.iter().any(|section| section.counting)
    }

    /// Whether the "still counting" hint is on (tests).
    #[cfg(test)]
    pub(super) fn hint_shown(&self) -> bool {
        self.slow && self.sections.iter().any(|section| section.counting)
    }

    /// What each section says, for the tests: `(line, warning, rows in the grid)`.
    #[cfg(test)]
    pub(super) fn texts(&self) -> Vec<(String, Option<String>, usize)> {
        self.sections
            .iter()
            .map(|section| (section.line.to_string(), section.warning.as_ref().map(ToString::to_string), section.rows))
            .collect()
    }

    /// What the boxes and notes say, for the tests: `(note, box note)`.
    #[cfg(test)]
    pub(super) fn notes(&self) -> Vec<(Option<String>, Option<String>)> {
        self.sections
            .iter()
            .map(|section| (section.note.as_ref().map(ToString::to_string), section.box_note.as_ref().map(ToString::to_string)))
            .collect()
    }
}

/// Reads one statement's count and first rows. `Err(Cancelled)` when the preview is gone.
async fn read(
    this: &gpui::WeakEntity<AffectedPreview>,
    root: &gpui::WeakEntity<AppRoot>,
    target: &Target,
    executor: &gpui::BackgroundExecutor,
    count_sql: String,
    rows_sql: String,
    cx: &mut gpui::AsyncWindowContext,
) -> Landed {
    let count = read_page(this, root, target, executor, count_sql, cx).await?;
    let count = count.cell_text(CellPosition { row: 0, column: 0 }).and_then(|text| text.trim().parse::<u64>().ok());
    let rows = read_page(this, root, target, executor, rows_sql, cx).await?;
    Ok((count, rows))
}

/// One read-only query, off the UI thread. The driver's page goes back as soon as its rows are ours.
async fn read_page(
    this: &gpui::WeakEntity<AffectedPreview>,
    root: &gpui::WeakEntity<AppRoot>,
    target: &Target,
    executor: &gpui::BackgroundExecutor,
    sql: String,
    cx: &mut gpui::AsyncWindowContext,
) -> Result<ArrowGridSource, DriverQueryError> {
    let gone = |_| DriverQueryError::Cancelled;
    #[cfg(test)]
    let _ = root.update(cx, |app, _| app.driver_runtime.preview_reads += 1);
    let PreparedDriverQuery { session, runner, operation } =
        root.update(cx, |app, _| app.begin_preview_query(target.tab_id, &target.connection)).map_err(gone)??;
    let request = preview_request(operation.operation_id, target.database.clone(), &sql);
    // From here a release of the preview cancels the query; before it there is nothing to cancel.
    this.update(cx, |this, _| this.operation = Some(operation)).map_err(gone)?;
    let page = executor.spawn(async move { smol::unblock(move || open_page(session, runner, request)).await }).await;
    let _ = this.update(cx, |this, _| this.operation = None);
    let (_, imported, guard) = page?;
    let source = ArrowGridSource::from_imported_page(imported).with_binary_display(BinaryDisplay::Hex);
    drop(guard);
    Ok(source)
}

/// What a preview sends: the text, to run in a read-only transaction (`payload.read_only`, capability
/// `query.read_only`): the server refuses a write with `query.read_only_violation`.
pub(super) fn preview_request(operation_id: OperationId, database: Option<String>, sql: &str) -> String {
    serde_json::to_string(&engine_driver_api::QueryRequest {
        kind: "sql".into(),
        operation_id,
        database,
        object: None,
        offset: 0,
        limit: PREVIEW_ROWS as u64,
        payload: serde_json::json!({"sql": sql, "read_only": true}),
    })
    .expect("a preview request serializes")
}

/// Opens the query's page and takes its rows (the body of `run_driver_sql_query`'s worker).
pub(super) fn open_page(
    session: Arc<engine_driver_host::SharedDriverSession>,
    runner: engine_driver_host::QueryRunner,
    request: String,
) -> Result<(engine_driver_api::QueryPage, engine_driver_host::ImportedArrowBatch, DriverPageGuard), DriverQueryError> {
    let session_for_guard = Arc::clone(&session);
    let page = runner.open_query(&request)?;
    // A poisoned lock after `open_query` must not leak the page.
    let mut session = session.lock().map_err(|_| {
        runner.release_page(page.page_id);
        DriverQueryError::SessionPoisoned
    })?;
    // Releases the page itself when the session is gone.
    session.adopt_page(&runner, page.page_id)?;
    let guard = DriverPageGuard::new(session_for_guard, page.page_id);
    let lease = session.acquire_arrow_page(page.page_id, 0, page.row_count)?;
    // SAFETY: the lease holds the Arrow C Data pair the driver exported for this page; import
    // consumes it once.
    let imported = unsafe { lease.into_record_batch() }?;
    Ok((page, imported, guard))
}

impl Render for AffectedPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let pending = self.sections.iter().any(|section| section.counting);
        div()
            .debug_selector(|| "affected-preview".into())
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(
                div()
                    .id("affected-sections")
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .max_h(px(SECTIONS_MAX_HEIGHT))
                    .overflow_y_scroll()
                    .children(self.sections.iter().enumerate().map(|(index, section)| section_view(index, section))),
            )
            // Its row is there from the start (empty until counting is slow, and after it) so nothing below moves.
            .when(self.reads, |view| {
                view.child(
                    div()
                        .debug_selector(|| "affected-slow".into())
                        .h(px(16.0))
                        .text_size(px(12.0))
                        .text_color(theme::text_muted())
                        .when(self.slow && pending, |hint| hint.child(self.slow_hint.clone())),
                )
            })
            .when_some(self.more.clone(), |view, more| {
                view.child(div().debug_selector(|| "affected-more".into()).text_size(px(12.0)).text_color(theme::text_muted()).child(more))
            })
            .when_some(self.script_note.clone(), |view, note| {
                view.child(div().debug_selector(|| "affected-script-note".into()).text_size(px(11.0)).text_color(theme::text_faint()).child(note))
            })
    }
}

fn section_view(index: usize, section: &Section) -> Div {
    let danger = theme::danger();
    div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .when_some(section.heading.clone(), |view, heading| {
            view.child(
                div()
                    .debug_selector(move || format!("affected-heading-{index}"))
                    .truncate()
                    .text_size(px(12.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme::text())
                    .child(heading),
            )
        })
        .when(!section.line.is_empty(), |view| {
            let colour = if section.unavailable { theme::text_faint() } else { theme::text_muted() };
            view.child(
                div()
                    .debug_selector(move || format!("affected-line-{index}"))
                    .text_size(px(12.0))
                    .text_color(colour)
                    .child(section.line.clone()),
            )
        })
        .when_some(section.note.clone(), |view, note| {
            view.child(
                div()
                    .debug_selector(move || format!("affected-note-{index}"))
                    .text_size(px(12.0))
                    .text_color(theme::text_muted())
                    .child(note),
            )
        })
        .when_some(section.warning.clone(), |view, warning| {
            view.child(
                div()
                    .debug_selector(move || format!("affected-warning-{index}"))
                    .px(px(8.0))
                    .py(px(6.0))
                    .rounded(px(4.0))
                    .border_1()
                    .border_color(super::navigator_ui::tinted(danger, 0.6))
                    .bg(super::navigator_ui::tinted(danger, 0.12))
                    .text_size(px(12.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(danger)
                    .child(warning),
            )
        })
        .when(section.box_height > 0.0, |view| {
            let framed = section.grid.is_some();
            view.child(
                div()
                    .debug_selector(move || format!("affected-rows-{index}"))
                    .h(px(section.box_height))
                    .flex_shrink_0()
                    .when(framed, |frame| frame.border_1().border_color(theme::border()).rounded(px(4.0)).overflow_hidden())
                    .when_some(section.grid.clone(), |frame, table| frame.child(DataTable::new(&table).stripe(true)))
                    .when_some(section.box_note.clone(), |frame, note| {
                        frame.child(
                            div()
                                .debug_selector(move || format!("affected-box-note-{index}"))
                                .truncate()
                                .px(px(4.0))
                                .text_size(px(12.0))
                                .text_color(theme::text_faint())
                                .child(note),
                        )
                    }),
            )
        })
}

/// The first rows of a preview: a virtualized `gpui-component` table over the driver's own page,
/// cells formatted by the result grid's source on demand.
pub(super) struct RowsDelegate {
    source: ArrowGridSource,
}

impl TableDelegate for RowsDelegate {
    fn columns_count(&self, _: &gpui::App) -> usize {
        self.source.column_count()
    }

    fn rows_count(&self, _: &gpui::App) -> usize {
        self.source.row_count()
    }

    fn column(&self, ix: usize, _: &gpui::App) -> Column {
        let name = self.source.column_name(ix).unwrap_or_default().to_owned();
        Column::new(format!("affected-column-{ix}"), name).width(px(120.0)).movable(false)
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let position = CellPosition { row: row_ix, column: col_ix };
        let (text, colour) = if self.source.cell_is_null(position) {
            (SharedString::from("NULL"), theme::text_faint())
        } else {
            (self.source.cell_text(position).unwrap_or_default(), theme::text())
        };
        div().size_full().truncate().text_size(px(12.0)).text_color(colour).child(text)
    }
}
