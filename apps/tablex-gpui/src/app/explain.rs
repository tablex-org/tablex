//! Explain in the SQL editor: the toolbar button (with Explain Analyze in its menu), the keyboard
//! shortcut and the palette entries all end up in [`AppRoot::explain_tab_query`].
//!
//! The statement is picked as Run picks it (the selection, else the statement under the cursor,
//! else the whole editor), built by `engine_plan::explain_sql` (which refuses what cannot be
//! explained: shown as the tab's normal inline error) and run through the tab's ordinary driver
//! route (`start_sql_run`): same session, cancel, lost-connection handling, Error Log and
//! confirmations. The result is read into a plan off the UI thread and shown by the Plan
//! presentation (`results::plan`); the tab's rows and paging are untouched.

use super::assistant::is_sql_editor;
use super::connection_policy::GatedRun;
use super::*;
use engine_driver_kit::sql::{classify, SqlLexOptions};
use engine_plan::{explain_sql, ExplainDialect, ExplainError, ExplainMode};

/// Why Explain Analyze is refused on a driver that cannot run a statement read-only.
const ANALYZE_NEEDS_READ_ONLY_DRIVER: &str =
    "Explain Analyze needs a driver that can run statements read-only (query.read_only); this one cannot";

/// Why Explain is refused for text whose reading depends on the (unknown) server version: the
/// server may read a `/*!N ... */` comment as code and so see more than one statement.
const VERSION_AMBIGUOUS: &str =
    "Explain cannot judge this statement: it has version-dependent /*! ... */ comments and the server version is unknown";

/// How `dialect`'s server reads statement text, for the classifier: the dialect's preset moved by
/// what the driver reported for this session (MySQL / MariaDB `NO_BACKSLASH_ESCAPES`,
/// `ANSI_QUOTES`, `MSSQL`; see `SqlLexOptions::with_session_dialect`) and, for MySQL / MariaDB,
/// the session's server version (a versioned `/*!N ... */` comment is code or comment depending
/// on it).
pub(super) fn lex_options(
    dialect: ExplainDialect,
    info: Option<&engine_driver_api::SqlDialectInfo>,
    server_version: Option<u32>,
) -> SqlLexOptions {
    let mut options = dialect.default_lex_options();
    if let Some(info) = info {
        options = options.with_session_dialect(info);
    }
    match (dialect, server_version) {
        (ExplainDialect::MySql | ExplainDialect::MariaDb, Some(version)) => {
            options.with_server_version(version)
        }
        _ => options,
    }
}

/// Exactly one statement and a pure read (no `SELECT ... INTO`, no data-modifying CTE), read the
/// way the server reads it. A lexer cannot see side effects of functions: the server-side
/// read-only run (`payload.read_only`) is what stops those.
#[cfg(test)]
pub(super) fn is_pure_read(sql: &str, lex: &SqlLexOptions) -> bool {
    classify(sql, lex).pageable
}

/// The explain dialect of a connection kind; `None` for MongoDB and Redis.
pub(super) fn explain_dialect(kind: ConnectionKind) -> Option<ExplainDialect> {
    match kind {
        ConnectionKind::Postgres => Some(ExplainDialect::Postgres),
        ConnectionKind::Mysql => Some(ExplainDialect::MySql),
        ConnectionKind::Mariadb => Some(ExplainDialect::MariaDb),
        ConnectionKind::Sqlite => Some(ExplainDialect::Sqlite),
        ConnectionKind::Clickhouse => Some(ExplainDialect::ClickHouse),
        ConnectionKind::Mongodb | ConnectionKind::Redis => None,
    }
}

impl AppRoot {
    /// The explain dialect of `tab_id` when it is a SQL editor on a connection that can explain.
    pub(super) fn explain_dialect_of(&self, tab_id: u64) -> Option<ExplainDialect> {
        let tab = self.workspace.tab(tab_id).filter(|tab| is_sql_editor(tab))?;
        self.saved_connections
            .iter()
            .find(|saved| saved.matches_id(&tab.connection_id))
            .and_then(|saved| explain_dialect(saved.kind))
    }

    /// Whether the active tab offers Explain (the palette lists the entries only then).
    pub(super) fn explain_offered(&self) -> bool {
        self.workspace
            .active_tab()
            .is_some_and(|tab| self.explain_dialect_of(tab.id).is_some())
    }

    /// What Explain explains: the selection, else the statement under the cursor, else the whole
    /// editor (Run's own choice, except that the "Run executes: whole script" setting does not
    /// apply, since a script cannot be explained).
    pub(super) fn explain_target(&self, tab_id: u64, cx: &gpui::App) -> String {
        let Some(script) = self.workspace.tab(tab_id).map(|tab| tab.sql.clone()) else {
            return String::new();
        };
        let Some(editor) = self.query_editors.by_tab.get(&tab_id) else {
            return script;
        };
        let (selection, cursor) = {
            let state = editor.read(cx);
            (state.selected_value().to_string(), state.cursor())
        };
        if !selection.trim().is_empty() {
            return selection;
        }
        crate::sql_script::statement_at(&script, cursor, &self.sql_lex(tab_id))
            .map_or_else(|| script.clone(), |range| script[range].to_string())
    }

    /// The tab's driver capabilities and SQL dialect, when its driver is loaded.
    fn explain_driver(&self, tab_id: u64) -> Option<&engine_driver_api::DriverCapabilities> {
        let tab = self.workspace.tab(tab_id)?;
        let saved = self
            .saved_connections
            .iter()
            .find(|saved| saved.matches_id(&tab.connection_id))?;
        self.driver_capabilities(&tab.connection_id, saved.kind.driver_id())
    }

    /// The lexer options for the tab's connection (see [`lex_options`]).
    pub(super) fn explain_lex(&self, tab_id: u64) -> Option<SqlLexOptions> {
        Some(self.lex_for(tab_id, self.explain_dialect_of(tab_id)?))
    }

    /// How the tab's server reads statement text, for the scanners that cut a script into
    /// statements and spot a write (the editor's "Run current statement", the read-only guard).
    /// A tab that is not on a SQL connection (never run through them) reads as PostgreSQL.
    pub(super) fn sql_lex(&self, tab_id: u64) -> SqlLexOptions {
        self.workspace
            .tab(tab_id)
            .and_then(|tab| {
                self.saved_connections
                    .iter()
                    .find(|saved| saved.matches_id(&tab.connection_id))
            })
            .and_then(|saved| explain_dialect(saved.kind))
            .map_or_else(SqlLexOptions::postgres, |dialect| self.lex_for(tab_id, dialect))
    }

    fn lex_for(&self, tab_id: u64, dialect: ExplainDialect) -> SqlLexOptions {
        let info = self
            .explain_driver(tab_id)
            .and_then(|capabilities| capabilities.sql_dialect.as_ref());
        let version = self
            .workspace
            .tab(tab_id)
            .and_then(|tab| self.driver_runtime.server_versions.get(&tab.connection_id))
            .copied();
        lex_options(dialect, info, version)
    }

    /// A loaded driver that does not advertise `query.read_only` cannot make an Analyze safe.
    pub(super) fn explain_driver_lacks_read_only(&self, tab_id: u64) -> bool {
        self.explain_driver(tab_id)
            .is_some_and(|capabilities| !capabilities.has(engine_driver_api::Capability::QueryReadOnly))
    }

    /// Why Explain Analyze is not available for the editor's current statement: the dialect has
    /// no analyze, the driver cannot run read-only, or the statement is not a single pure read
    /// (analyze would execute it). `None` when it is available.
    pub(super) fn explain_analyze_block(&self, tab_id: u64, cx: &gpui::App) -> Option<String> {
        let dialect = self.explain_dialect_of(tab_id)?;
        if !dialect.supports_analyze() {
            return Some(ExplainError::AnalyzeUnsupported(dialect).to_string());
        }
        if self.explain_driver_lacks_read_only(tab_id) {
            return Some(ANALYZE_NEEDS_READ_ONLY_DRIVER.to_string());
        }
        let lex = self.explain_lex(tab_id)?;
        let classified = classify(&self.explain_target(tab_id, cx), &lex);
        if classified.version_ambiguous {
            return Some(VERSION_AMBIGUOUS.to_string());
        }
        (!classified.pageable).then(|| ExplainError::AnalyzeRequiresReadOnly.to_string())
    }

    /// Explain (or Explain Analyze) the editor's statement on the active tab.
    pub(super) fn explain_active_tab(&mut self, mode: ExplainMode, cx: &mut Context<Self>) {
        if let Some(tab_id) = self.workspace.active_tab().map(|tab| tab.id) {
            self.explain_tab_query(tab_id, mode, cx);
        }
    }

    pub(super) fn explain_tab_query(&mut self, tab_id: u64, mode: ExplainMode, cx: &mut Context<Self>) {
        let sql = self.explain_target(tab_id, cx);
        if self.prompt_parameters(tab_id, &sql, AfterParams::Explain(mode), cx) {
            return;
        }
        self.explain_bound(tab_id, mode, sql, cx);
    }

    /// Explain of `sql`, its parameters (if any) already bound.
    pub(super) fn explain_bound(&mut self, tab_id: u64, mode: ExplainMode, sql: String, cx: &mut Context<Self>) {
        let Some(run) = self.explain_statement(tab_id, mode, &sql, cx) else {
            return;
        };
        // Analyze executes the statement: the connection's execute confirmation applies to it.
        if mode == ExplainMode::Analyze && self.gate_statement(tab_id, &sql, GatedRun::Explain(mode), cx) {
            return;
        }
        self.start_sql_run(tab_id, run.0, Some(0), Some(run.1), cx);
    }

    /// Starts the Explain the user confirmed.
    pub(super) fn explain_confirmed(&mut self, tab_id: u64, mode: ExplainMode, sql: &str, cx: &mut Context<Self>) {
        if let Some(run) = self.explain_statement(tab_id, mode, sql, cx) {
            self.start_sql_run(tab_id, run.0, Some(0), Some(run.1), cx);
        }
    }

    /// The EXPLAIN statement for `sql` and how its result is read. A statement `explain_sql`
    /// refuses (empty, several statements, already an EXPLAIN, Analyze of a write or on a dialect
    /// without it) becomes the tab's inline error and gives `None`.
    pub(super) fn explain_statement(
        &mut self,
        tab_id: u64,
        mode: ExplainMode,
        sql: &str,
        cx: &mut Context<Self>,
    ) -> Option<(String, ExplainRun)> {
        let dialect = self.explain_dialect_of(tab_id)?;
        if mode == ExplainMode::Analyze && self.explain_driver_lacks_read_only(tab_id) {
            self.refuse_statement(tab_id, sql, ANALYZE_NEEDS_READ_ONLY_DRIVER.to_string(), cx);
            return None;
        }
        let lex = self.explain_lex(tab_id)?;
        let classified = classify(sql, &lex);
        if classified.version_ambiguous {
            self.refuse_statement(tab_id, sql, VERSION_AMBIGUOUS.to_string(), cx);
            return None;
        }
        let read_only = classified.pageable;
        match explain_sql(dialect, mode, sql, read_only, &lex) {
            // Analyze, and the Estimate of a pure read, run in the driver's read-only execution. The
            // Estimate of a single write does not: MySQL / MariaDB refuse the statement itself in
            // a READ ONLY transaction, and an EXPLAIN executes nothing (`explain_sql` has just
            // checked it is one statement, read with the session's own lexer options).
            Ok(statement) => Some((
                statement.sql,
                ExplainRun {
                    dialect,
                    mode,
                    read_only: mode == ExplainMode::Analyze || read_only,
                },
            )),
            Err(error) => {
                self.refuse_statement(tab_id, sql, error.to_string(), cx);
                None
            }
        }
    }

    /// The editor header's Explain button: Explain on click, Explain Analyze in its menu.
    pub(super) fn render_explain_button(&self, tab_id: u64, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.explain_dialect_of(tab_id)?;
        let can_run = self
            .workspace
            .runtime(tab_id)
            .is_some_and(|runtime| runtime.query.can_run());
        let root = cx.entity().downgrade();
        let button = DropdownButton::new(("explain-group", tab_id))
            .small()
            .button(
                Button::new(("explain-query", tab_id))
                    .small()
                    .debug_selector(|| "explain-query".into())
                    .label("Explain")
                    .compact()
                    .ghost()
                    .disabled(!can_run)
                    .tooltip(shortcuts::with_label(
                        "Show the execution plan of the statement",
                        &self.settings.shortcuts,
                        "editor.explain",
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.explain_tab_query(tab_id, ExplainMode::Estimate, cx);
                    })),
            )
            .ghost()
            .disabled(!can_run)
            .dropdown_menu(move |menu, _, cx| {
                // Read when the menu opens, not on every render: it scans the editor text.
                let block = root
                    .read_with(&*cx, |this, cx| this.explain_analyze_block(tab_id, cx))
                    .ok()
                    .flatten();
                let (explain_root, analyze_root) = (root.clone(), root.clone());
                menu.item(
                    PopupMenuItem::element(|_, _| {
                        div().id("explain-menu-explain").debug_selector(|| "explain-menu-explain".into()).child("Explain")
                    })
                    .on_click(move |_, _, app| {
                        let _ = explain_root.update(app, |this, cx| {
                            this.explain_tab_query(tab_id, ExplainMode::Estimate, cx)
                        });
                    }),
                )
                .item(
                    PopupMenuItem::element({
                        let reason = block.clone();
                        move |_, _| {
                            let row = div()
                                .id("explain-menu-analyze")
                                .debug_selector(|| "explain-menu-analyze".into())
                                .child("Explain Analyze");
                            match reason.clone() {
                                Some(reason) => row
                                    .tooltip(move |window, cx| {
                                        gpui_component::tooltip::Tooltip::new(reason.clone()).build(window, cx)
                                    })
                                    .into_any_element(),
                                None => row.into_any_element(),
                            }
                        }
                    })
                    .disabled(block.is_some())
                    .on_click(move |_, _, app| {
                        let _ = analyze_root.update(app, |this, cx| {
                            this.explain_tab_query(tab_id, ExplainMode::Analyze, cx)
                        });
                    }),
                )
            });
        Some(
            div()
                .debug_selector(|| "explain-dropdown".into())
                .child(button)
                .into_any_element(),
        )
    }
}
