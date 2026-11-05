//! Static palette actions. Each one calls the existing `AppRoot` operation
//! behind the matching toolbar button, shortcut or sidebar command; nothing
//! here re-implements behavior.

use super::super::shortcuts::{self, Overrides};
use super::super::tab_bar::TabIcon;
use super::super::AppRoot;
use crate::panes::Edge;
use crate::platform::primary_label;
use gpui::{Context, Window};
use std::borrow::Cow;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::app) enum StaticAction {
    NewQuery,
    NewConnection,
    SaveQuery,
    SaveSqlFileAs,
    OpenSqlFile,
    RunSqlFile,
    BackupDatabase,
    RestoreDatabase,
    UsersAndPrivileges,
    FormatSql,
    ExplainPlan,
    ExplainPlanAnalyze,
    CommitTransaction,
    RollbackTransaction,
    ToggleAutoCommit,
    PendingTransactions,
    FindInResults,
    CloseTab,
    DriverManager,
    Settings,
    ErrorLog,
    Assistant,
    ExplainQuery,
    FixQuery,
    OptimiseQuery,
    FocusSidebar,
    ToggleSidebar,
    RevealActiveTab,
    CollapseSidebar,
    ViewDiagram,
    SplitEditorRight,
    SplitEditorDown,
    FocusNextPane,
    ToggleTabRows,
    NextTabGroup,
    PreviousTabGroup,
    About,
    Quit,
}

impl StaticAction {
    pub(super) const ALL: [Self; 38] = [
        Self::NewQuery,
        Self::NewConnection,
        Self::SaveQuery,
        Self::SaveSqlFileAs,
        Self::OpenSqlFile,
        Self::RunSqlFile,
        Self::BackupDatabase,
        Self::RestoreDatabase,
        Self::UsersAndPrivileges,
        Self::FormatSql,
        Self::ExplainPlan,
        Self::ExplainPlanAnalyze,
        Self::CommitTransaction,
        Self::RollbackTransaction,
        Self::ToggleAutoCommit,
        Self::PendingTransactions,
        Self::FindInResults,
        Self::CloseTab,
        Self::DriverManager,
        Self::Settings,
        Self::ErrorLog,
        Self::Assistant,
        Self::ExplainQuery,
        Self::FixQuery,
        Self::OptimiseQuery,
        Self::FocusSidebar,
        Self::ToggleSidebar,
        Self::RevealActiveTab,
        Self::CollapseSidebar,
        Self::ViewDiagram,
        Self::SplitEditorRight,
        Self::SplitEditorDown,
        Self::FocusNextPane,
        Self::ToggleTabRows,
        Self::NextTabGroup,
        Self::PreviousTabGroup,
        Self::About,
        Self::Quit,
    ];

    pub(super) fn title(self) -> &'static str {
        match self {
            Self::NewQuery => "New Query",
            Self::NewConnection => "New Connection",
            Self::SaveQuery => "Save Script",
            Self::SaveSqlFileAs => "Save SQL File As",
            Self::OpenSqlFile => "Open SQL File",
            Self::RunSqlFile => "Run SQL File",
            Self::BackupDatabase => "Backup Database",
            Self::RestoreDatabase => "Restore Database",
            Self::UsersAndPrivileges => "Users & Privileges",
            Self::FormatSql => "Format SQL",
            Self::ExplainPlan => "Explain",
            Self::ExplainPlanAnalyze => "Explain Analyze",
            Self::CommitTransaction => "Commit Transaction",
            Self::RollbackTransaction => "Roll Back Transaction",
            Self::ToggleAutoCommit => "Toggle Auto-commit / Manual",
            Self::PendingTransactions => "Pending Transactions",
            Self::FindInResults => "Find in Results",
            Self::CloseTab => "Close Tab",
            Self::DriverManager => "Driver Manager",
            Self::Settings => "Settings",
            Self::ErrorLog => "Error Log",
            Self::Assistant => "AI Assistant",
            Self::ExplainQuery => "Assistant: Explain Query",
            Self::FixQuery => "Assistant: Fix Query",
            Self::OptimiseQuery => "Assistant: Optimise Query",
            Self::FocusSidebar => "Focus Sidebar",
            Self::ToggleSidebar => "Toggle Sidebar",
            Self::RevealActiveTab => "Reveal Active Tab in Sidebar",
            Self::CollapseSidebar => "Collapse Sidebar Tree",
            Self::ViewDiagram => "View Diagram",
            Self::SplitEditorRight => "Split Editor Right",
            Self::SplitEditorDown => "Split Editor Down",
            Self::FocusNextPane => "Focus Next Pane",
            Self::ToggleTabRows => "Show Tabs in a Second Row",
            Self::NextTabGroup => "Next Tab Group",
            Self::PreviousTabGroup => "Previous Tab Group",
            Self::About => "About TableX",
            Self::Quit => "Quit TableX",
        }
    }

    /// One-line description; the shortcut shown is the one in force (`overrides`).
    pub(in crate::app) fn detail(self, overrides: &Overrides) -> Cow<'static, str> {
        let hinted = |text: &str, id: &str| -> Cow<'static, str> {
            match shortcuts::label(overrides, id) {
                Some(label) => format!("{text}  {label}").into(),
                None => text.to_owned().into(),
            }
        };
        match self {
            Self::NewQuery => "Open a query editor for the selected sidebar node".into(),
            Self::NewConnection => "Add a database connection".into(),
            Self::SaveQuery => hinted("Save the active query (its file, when it has one)", "editor.save_script"),
            Self::SaveSqlFileAs => hinted("Write the query to a .sql file and keep the tab bound to it", "editor.save_sql_file_as"),
            Self::OpenSqlFile => hinted("Open a .sql file in a query tab", "editor.open_sql_file"),
            Self::RunSqlFile => "Run a .sql file of any size on a connection, without opening it".into(),
            Self::BackupDatabase => "Back a PostgreSQL, MySQL or MariaDB database up with pg_dump or mysqldump".into(),
            Self::RestoreDatabase => "Restore a backup with pg_restore, or an SQL script with Run SQL File".into(),
            Self::UsersAndPrivileges => "List, create, alter and drop the roles of PostgreSQL or the users of MySQL and MariaDB, and grant privileges, through a reviewed change-set".into(),
            Self::FormatSql => hinted("Re-lay out the selected statements, or the whole editor", "editor.format_sql"),
            Self::ExplainPlan => hinted("Show the execution plan of the statement", "editor.explain"),
            Self::ExplainPlanAnalyze => hinted(
                "Run the statement and show its plan with timings (read-only statements)",
                "editor.explain_analyze",
            ),
            Self::CommitTransaction => hinted("Commit the open transaction of the query tab", "editor.commit"),
            Self::RollbackTransaction => hinted("Roll back the open transaction of the query tab", "editor.rollback"),
            Self::ToggleAutoCommit => hinted(
                "Switch the query tab between auto-commit and manual commit",
                "editor.toggle_auto_commit",
            ),
            Self::PendingTransactions => "List every open transaction, and commit or roll back each or the selected".into(),
            Self::FindInResults => hinted("Search the result grid", "results.find"),
            Self::CloseTab => hinted("Close the active tab", "workspace.close_tab"),
            Self::DriverManager => "Install and manage database drivers".into(),
            Self::Settings => hinted("Open application settings", "workspace.open_settings"),
            Self::ErrorLog => hinted("Show the event and error log", "workspace.toggle_error_log"),
            Self::Assistant => hinted("Show or hide the AI assistant panel", "workspace.toggle_assistant"),
            Self::ExplainQuery => {
                "Ask the assistant to explain the selected SQL, or the whole editor".into()
            }
            Self::FixQuery => {
                "Ask the assistant to fix the SQL whose last run failed, with the error".into()
            }
            Self::OptimiseQuery => {
                "Ask the assistant to make the selected SQL, or the whole editor, faster".into()
            }
            Self::FocusSidebar => "Move keyboard focus to the connection tree".into(),
            Self::ToggleSidebar => "Show or hide the sidebar".into(),
            Self::RevealActiveTab => "Locate the active tab's object in the tree".into(),
            Self::CollapseSidebar => "Collapse every expanded tree node".into(),
            Self::ViewDiagram => format!(
                "ER diagram of the selected schema  ({} on a database or schema)",
                primary_label(false, "Enter")
            )
            .into(),
            Self::SplitEditorRight => "Move the active tab into a new pane on the right".into(),
            Self::SplitEditorDown => "Move the active tab into a new pane below".into(),
            Self::FocusNextPane => "Move focus to the next editor pane".into(),
            Self::ToggleTabRows => {
                "Group chips on one row, the active group's tabs on a second (groups tabs by connection)".into()
            }
            Self::NextTabGroup => {
                hinted("Show the next connection group of tabs", "workspace.next_tab_group")
            }
            Self::PreviousTabGroup => {
                hinted("Show the previous connection group of tabs", "workspace.previous_tab_group")
            }
            Self::About => "Show the application version".into(),
            Self::Quit => hinted("Close TableX", "app.quit"),
        }
    }

    /// Extra matched-only terms.
    pub(super) fn keywords(self) -> &'static str {
        match self {
            Self::NewQuery => "sql editor console script",
            Self::NewConnection => "add database server connect",
            Self::SaveQuery => "store file",
            Self::SaveSqlFileAs => "write export file disk",
            Self::OpenSqlFile => "load import file disk script",
            Self::RunSqlFile => "execute large dump restore script file stream",
            Self::BackupDatabase => "dump export pg_dump mysqldump mariadb-dump save copy archive",
            Self::RestoreDatabase => "import load pg_restore archive dump recover",
            Self::UsersAndPrivileges => "users roles accounts permissions privileges grant revoke password create user create role drop user security access",
            Self::FormatSql => "format beautify prettify indent reformat sql query",
            Self::ExplainPlan => "explain plan execution query cost estimate",
            Self::ExplainPlanAnalyze => "explain analyze plan execution query timing actual rows",
            Self::CommitTransaction => "commit transaction save changes manual",
            Self::RollbackTransaction => "rollback roll back transaction undo discard changes manual",
            Self::ToggleAutoCommit => "auto commit autocommit manual transaction mode",
            Self::PendingTransactions => "open transactions uncommitted commit rollback roll back pending tabs",
            Self::FindInResults => "search grid filter",
            Self::CloseTab => "dismiss",
            Self::DriverManager => "drivers plugins install",
            Self::Settings => "preferences options configuration appearance editor fonts",
            Self::ErrorLog => "errors warnings logs incidents",
            Self::Assistant => "ai chat claude llm ask sql help copilot",
            Self::ExplainQuery => "ai assistant explain describe sql query what does",
            Self::FixQuery => "ai assistant fix repair error failed sql query",
            Self::OptimiseQuery => "ai assistant optimise optimize faster improve slow sql query",
            Self::FocusSidebar => "navigation tree navigator",
            Self::ToggleSidebar => "hide show panel",
            Self::RevealActiveTab => "locate select sidebar",
            Self::CollapseSidebar => "collapse all tree",
            Self::ViewDiagram => "er erd relationships foreign keys schema database",
            Self::SplitEditorRight => "split pane column side by side layout",
            Self::SplitEditorDown => "split pane row stacked layout",
            Self::FocusNextPane => "switch pane group window",
            Self::ToggleTabRows => "tab strip bar rows chips group two lines layout",
            Self::NextTabGroup => "switch connection chip cycle",
            Self::PreviousTabGroup => "switch connection chip cycle back",
            Self::About => "version info help",
            Self::Quit => "exit close application",
        }
    }

    pub(super) fn icon(self) -> TabIcon {
        let path = match self {
            Self::NewQuery => "icons/square-terminal.svg",
            Self::NewConnection => "icons/plus.svg",
            Self::SaveQuery => "icons/download.svg",
            Self::SaveSqlFileAs => "icons/download.svg",
            Self::OpenSqlFile => "icons/folder-open.svg",
            Self::RunSqlFile => "icons/play.svg",
            Self::BackupDatabase => "icons/download.svg",
            Self::RestoreDatabase => "icons/upload.svg",
            Self::UsersAndPrivileges => "icons/key-round.svg",
            Self::FormatSql => "icons/braces.svg",
            Self::ExplainPlan | Self::ExplainPlanAnalyze => "icons/network.svg",
            Self::CommitTransaction => "icons/check.svg",
            Self::RollbackTransaction => "icons/undo-2.svg",
            Self::ToggleAutoCommit | Self::PendingTransactions => "icons/database.svg",
            Self::FindInResults => "icons/search.svg",
            Self::CloseTab => "icons/close.svg",
            Self::DriverManager => "icons/settings-2.svg",
            Self::Settings => "icons/settings.svg",
            Self::ErrorLog => "icons/triangle-alert.svg",
            Self::Assistant | Self::ExplainQuery | Self::FixQuery | Self::OptimiseQuery => {
                "icons/wand-sparkles.svg"
            }
            Self::FocusSidebar => "icons/layout-dashboard.svg",
            Self::ToggleSidebar => "icons/chevrons-left.svg",
            Self::RevealActiveTab => "icons/locate-fixed.svg",
            Self::CollapseSidebar => "icons/chevrons-down-up.svg",
            Self::ViewDiagram => "icons/network.svg",
            Self::SplitEditorRight => "icons/panel-right.svg",
            Self::SplitEditorDown => "icons/panel-bottom.svg",
            Self::FocusNextPane => "icons/columns-3.svg",
            Self::ToggleTabRows => "icons/rows-3.svg",
            Self::NextTabGroup => "icons/chevrons-up-down.svg",
            Self::PreviousTabGroup => "icons/chevrons-up-down.svg",
            Self::About => "icons/info.svg",
            Self::Quit => "icons/close.svg",
        };
        TabIcon::Plain(path)
    }

    pub(in crate::app) fn run(
        self,
        app: &mut AppRoot,
        window: &mut Window,
        cx: &mut Context<AppRoot>,
    ) {
        match self {
            Self::NewQuery => app.open_scoped_or_generic_query(cx),
            Self::NewConnection => app.open_connection_dialog(None, window, cx),
            Self::SaveQuery => app.save_active_sql(window, cx),
            Self::SaveSqlFileAs => app.save_sql_file_as(window, cx),
            Self::OpenSqlFile => app.open_sql_file(window, cx),
            Self::RunSqlFile => app.run_sql_file_from_context(window, cx),
            Self::BackupDatabase => app.open_backup_from_context(super::super::backup::Mode::Backup, window, cx),
            Self::RestoreDatabase => app.open_backup_from_context(super::super::backup::Mode::Restore, window, cx),
            Self::UsersAndPrivileges => app.open_accounts_from_context(window, cx),
            Self::FormatSql => app.format_sql(window, cx),
            Self::ExplainPlan => app.explain_active_tab(engine_plan::ExplainMode::Estimate, cx),
            Self::ExplainPlanAnalyze => app.explain_active_tab(engine_plan::ExplainMode::Analyze, cx),
            Self::CommitTransaction => app.end_active_transaction(super::super::transaction::TxEnd::Commit, window, cx),
            Self::RollbackTransaction => app.end_active_transaction(super::super::transaction::TxEnd::Rollback, window, cx),
            Self::ToggleAutoCommit => app.toggle_commit_mode(window, cx),
            Self::PendingTransactions => app.open_pending_transactions(window, cx),
            Self::FindInResults => app.open_active_grid_find(window, cx),
            Self::CloseTab => app.close_active_tab_action(window, cx),
            Self::DriverManager => app.open_driver_manager(cx),
            Self::Settings => app.open_settings(window, cx),
            Self::ErrorLog => app.toggle_error_log(window, cx),
            Self::Assistant => app.toggle_assistant(window, cx),
            Self::ExplainQuery => {
                app.assistant_editor_ask(super::super::assistant::EditorAsk::Explain, window, cx)
            }
            Self::FixQuery => {
                app.assistant_editor_ask(super::super::assistant::EditorAsk::Fix, window, cx)
            }
            Self::OptimiseQuery => {
                app.assistant_editor_ask(super::super::assistant::EditorAsk::Optimise, window, cx)
            }
            Self::FocusSidebar => {
                app.shell.sidebar_collapsed = false;
                window.focus(&app.sidebar.focus, cx);
                cx.notify();
            }
            Self::ToggleSidebar => {
                app.shell.sidebar_collapsed = !app.shell.sidebar_collapsed;
                cx.notify();
            }
            Self::RevealActiveTab => {
                app.shell.sidebar_collapsed = false;
                app.locate_active_sidebar_tab(cx);
            }
            Self::CollapseSidebar => app.collapse_navigator(cx),
            Self::ViewDiagram => app.open_diagram_for_selection(cx),
            Self::SplitEditorRight => app.split_editor(Edge::Right, window, cx),
            Self::SplitEditorDown => app.split_editor(Edge::Bottom, window, cx),
            Self::FocusNextPane => app.focus_next_pane(cx),
            Self::ToggleTabRows => app.toggle_tab_rows(cx),
            Self::NextTabGroup => app.step_tab_group(1, cx),
            Self::PreviousTabGroup => app.step_tab_group(-1, cx),
            Self::About => app.open_about(window, cx),
            Self::Quit => {
                if !app.request_quit(window, cx) {
                    cx.quit();
                }
            }
        }
    }
}
