//! MongoDB shell scripts in a script tab.
//!
//! A script the fast path cannot take (`inspect_read_script`: one `find` / `findOne`, which stays
//! an editable paged grid) is a shell script, run by the optional MongoDB Shell plugin:
//!
//! 1. the plugin must be installed and its driver recent enough ([`PluginState`]); if not, a
//!    dialog offers the install and the tab waits for it ([`ShellWait`]), then the script runs;
//! 2. the connection must be open (a closed one is opened first, like for any query);
//! 3. a connection that asks for confirmation shows the script before it runs;
//! 4. the run (`engine::ShellJob`) happens on a blocking thread; its reply is checked
//!    (`crate::mongo_shell::check_reply`) before anything of it is shown.
//!
//! Stop, timeouts, the history and the tab's status follow the other query kinds.

mod engine;
mod prompt;
mod view;

pub(in crate::app) use engine::{HostEngine, ShellEngine};
#[cfg(test)]
pub(in crate::app) use engine::{RunningShell, ShellLaunch};
#[cfg(all(test, feature = "bench"))]
use engine::prepare_reply;
use engine::{ReadyReply, ShellJob, ShellRunError, StopSlot};
pub(in crate::app) use prompt::PluginState;

use super::mongo_script::queue_pending_mongo_script;
use super::*;
use crate::mongo_shell::{failure_label, failure_text, CheckedReply, MoreCursor, ShellView};
use engine_driver_api::mongo_shell::ShellRequest;
use engine_driver_host::{LoaderError, ShellHostError, MONGO_SHELL_TOOL_ID};

/// Why the install a waiting script was to follow did not happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) enum AddonInstallError {
    /// The driver it needs was not installed (cancelled, failed, or dropped from the queue).
    DriverNotInstalled,
    /// The catalog no longer offers the add-on for this system.
    NotOffered,
    /// Another tool install was running.
    Busy,
    Cancelled,
    /// The catalog it came from is no longer trusted.
    CatalogNotTrusted,
    /// Nothing started (the catalog changed, a job was running).
    NotStarted,
    /// The install failed; the failure's own message.
    Failed(String),
}

impl std::fmt::Display for AddonInstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DriverNotInstalled => f.write_str("its driver was not installed"),
            Self::NotOffered => f.write_str("the catalog does not offer it"),
            Self::Busy => f.write_str("another install is running; try again"),
            Self::Cancelled => f.write_str("the install was cancelled"),
            Self::CatalogNotTrusted => f.write_str("the catalog is no longer trusted"),
            Self::NotStarted => f.write_str("the install could not start; open the Driver Manager"),
            Self::Failed(message) => f.write_str(message),
        }
    }
}

/// The shell state of the app: the engine that starts runs, the scripts waiting for the plugin and
/// the runs in flight.
pub(in crate::app) struct ShellUi {
    pub(in crate::app) engine: Arc<dyn ShellEngine>,
    waits: HashMap<u64, ShellWait>,
    runs: HashMap<u64, ShellRunHandle>,
    /// The attempt number of each tab's latest Run: an install that finishes later only runs the
    /// script of the attempt that asked for it.
    attempts: HashMap<u64, u64>,
    next_attempt: u64,
}

#[cfg(test)]
impl ShellUi {
    pub(in crate::app) fn waits_len(&self) -> usize {
        self.waits.len()
    }

    pub(in crate::app) fn runs_len(&self) -> usize {
        self.runs.len()
    }
}

impl Default for ShellUi {
    fn default() -> Self {
        Self {
            engine: Arc::new(HostEngine),
            waits: HashMap::new(),
            runs: HashMap::new(),
            attempts: HashMap::new(),
            next_attempt: 0,
        }
    }
}

/// A script that waits for the plugin to be installed. The tab shows a running run meanwhile.
#[derive(Debug, Clone)]
struct ShellWait {
    script: String,
    selection: bool,
    attempt: u64,
    run: crate::query::QueryRunId,
    /// Where the script was meant to run: a tab that moved to another database or connection
    /// meanwhile does not run it there.
    database: String,
    connection: String,
}

struct ShellRunHandle {
    run: crate::query::QueryRunId,
    slot: Arc<StopSlot>,
}

/// One script to run.
#[derive(Debug, Clone)]
struct ShellScript {
    tab_id: u64,
    script: String,
    selection: bool,
    database: String,
}

/// What the run of a tab needs once its reply arrives.
struct RunContext {
    tab_id: u64,
    run: crate::query::QueryRunId,
    connection: String,
    epoch: u64,
    script: String,
    database: String,
    /// Records the run in the query history (a "More" is not a run of its own).
    history: bool,
    page_size: i64,
}

impl AppRoot {
    /// A script of a script tab that is not a single find: runs it on the plugin, or asks to
    /// install it first. `pending_run` is the run a queued script was given while its connection
    /// opened (it is withdrawn here and a new one begins when the script really starts).
    #[allow(clippy::too_many_arguments)]
    pub(in crate::app) fn run_mongo_shell_script(
        &mut self,
        tab_id: u64,
        script: String,
        selection: bool,
        database: String,
        pending_run: Option<crate::query::QueryRunId>,
        cx: &mut Context<Self>,
    ) {
        if let (Some(run), Some(runtime)) = (pending_run, self.workspace.runtime_mut(tab_id)) {
            runtime.query.withdraw(run);
        }
        self.mongo_shell.next_attempt += 1;
        let attempt = self.mongo_shell.next_attempt;
        self.mongo_shell.attempts.insert(tab_id, attempt);
        let shell_script = ShellScript { tab_id, script, selection, database };
        match self.shell_plugin_state() {
            PluginState::Ready => self.shell_connect_then_run(shell_script, cx),
            // An install of the plugin is already running: the script follows it.
            PluginState::Installing => self.wait_for_shell_plugin(shell_script, attempt, cx),
            state => self.prompt_shell_install(shell_script, attempt, state, cx),
        }
    }

    /// The scripts of tabs waiting for the plugin start (or fail) now that its install ended.
    pub(in crate::app) fn on_addon_install_ended(
        &mut self,
        tool: &str,
        result: Result<(), AddonInstallError>,
        cx: &mut Context<Self>,
    ) {
        if tool != MONGO_SHELL_TOOL_ID {
            return;
        }
        let waits = std::mem::take(&mut self.mongo_shell.waits);
        for (tab_id, wait) in waits {
            // Only a tab still waiting on this very run, with the script of this very attempt.
            let live = self.workspace.runtime(tab_id).is_some_and(|runtime| {
                matches!(
                    runtime.query.status(),
                    crate::query::QueryStatus::Running { run } if *run == wait.run
                )
            });
            let same_attempt = self.mongo_shell.attempts.get(&tab_id) == Some(&wait.attempt);
            let same_script = self.workspace.tab(tab_id).is_some_and(|tab| {
                tab.query_mode.is_mongo_script()
                    && (wait.selection || tab.sql == wait.script)
                    && tab.database == wait.database
                    && tab.connection_id == wait.connection
            });
            if !live || !same_attempt || !same_script {
                if live {
                    if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
                        runtime.query.withdraw(wait.run);
                    }
                }
                continue;
            }
            match &result {
                Ok(()) => {
                    if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
                        runtime.query.withdraw(wait.run);
                    }
                    self.run_mongo_shell_script(
                        tab_id,
                        wait.script,
                        wait.selection,
                        wait.database,
                        None,
                        cx,
                    );
                }
                Err(reason) => {
                    let message = format!("Could not install the MongoDB Shell plugin: {reason}");
                    self.fail_mongo_script(tab_id, &message, cx);
                }
            }
        }
        cx.notify();
    }

    /// An install of the plugin started: waiting tabs repaint with its progress.
    pub(in crate::app) fn on_addon_install_started(&mut self, cx: &mut Context<Self>) {
        self.notify_waiting_shell_tabs(cx);
    }

    pub(in crate::app) fn on_addon_install_progress(&mut self, tool: &str, cx: &mut Context<Self>) {
        if tool == MONGO_SHELL_TOOL_ID {
            self.notify_waiting_shell_tabs(cx);
        }
    }

    fn notify_waiting_shell_tabs(&mut self, cx: &mut Context<Self>) {
        let tabs: Vec<u64> = self.mongo_shell.waits.keys().copied().collect();
        for tab_id in tabs {
            self.notify_tab(tab_id, cx);
        }
    }

    /// Registers `script` as waiting for the plugin: the tab runs (so Cancel is there) until the
    /// install ends.
    fn wait_for_shell_plugin(&mut self, script: ShellScript, attempt: u64, cx: &mut Context<Self>) {
        let ShellScript { tab_id, script, selection, database } = script;
        let connection = self
            .workspace
            .tab(tab_id)
            .map(|tab| tab.connection_id.clone())
            .unwrap_or_default();
        let Some(run) = self
            .workspace
            .runtime_mut(tab_id)
            .and_then(|runtime| runtime.query.begin())
        else {
            return;
        };
        self.mongo_shell
            .waits
            .insert(tab_id, ShellWait { script, selection, attempt, run, database, connection });
        self.notify_tab(tab_id, cx);
    }

    /// What Cancel does to a shell script: forgets it if it waits for the plugin, stops it if it
    /// runs. Off the UI thread, since it may have to kill a process.
    pub(in crate::app) fn stop_shell_run(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        self.mongo_shell.waits.remove(&tab_id);
        let Some(handle) = self.mongo_shell.runs.remove(&tab_id) else {
            return;
        };
        let slot = handle.slot;
        cx.background_executor()
            .spawn(async move { smol::unblock(move || slot.stop()).await })
            .detach();
    }

    /// Opens the connection if needed, confirms if the connection asks, then starts the script.
    fn shell_connect_then_run(&mut self, script: ShellScript, cx: &mut Context<Self>) {
        let tab_id = script.tab_id;
        let Some(connection) = self.workspace.tab(tab_id).map(|tab| tab.connection_id.clone())
        else {
            return;
        };
        let live = self.driver_runtime.sessions.contains_key(&connection)
            || !self.mongo_shell.engine.needs_session();
        if !live {
            let Some(saved) = self
                .saved_connections
                .iter()
                .find(|saved| saved.matches_id(&connection) && !saved.disabled)
                .cloned()
            else {
                self.fail_mongo_script(tab_id, "MongoDB connection is unavailable", cx);
                return;
            };
            if self.scenario_ui.service.is_some() {
                self.fail_mongo_script(
                    tab_id,
                    "MongoDB shell scripts are unavailable in a UI scenario",
                    cx,
                );
                return;
            }
            self.ensure_driver_connected(saved, None, None, cx);
            let connecting = matches!(
                self.sessions.status(&connection),
                ConnectionStatus::Connecting
            );
            let epoch = self.sessions.generation(&connection);
            match (connecting, epoch) {
                (true, Some(epoch)) => {
                    queue_pending_mongo_script(
                        &mut self.workspace,
                        &mut self.query_tasks,
                        tab_id,
                        &connection,
                        epoch,
                        &script.script,
                        script.selection,
                    );
                    cx.notify();
                }
                _ => self.fail_mongo_script(
                    tab_id,
                    "Could not connect MongoDB driver; check connection status",
                    cx,
                ),
            }
            return;
        }
        // A connection of a type that asks first shows the script; the answer starts it.
        if self.gate_shell_script(tab_id, &script.script, cx) {
            return;
        }
        self.start_shell_run(script.tab_id, script.script, script.database, None, true, cx);
    }

    /// The user confirmed a script of a production-type connection.
    pub(in crate::app) fn run_confirmed_shell_script(
        &mut self,
        tab_id: u64,
        script: String,
        cx: &mut Context<Self>,
    ) {
        let Some(database) = self.workspace.tab(tab_id).map(|tab| tab.database.clone()) else {
            return;
        };
        self.start_shell_run(tab_id, script, database, None, true, cx);
    }

    /// "More": the next page of the cursor the last run left open (mongosh `it`).
    pub(in crate::app) fn shell_more(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let Some(more) = self
            .workspace
            .runtime(tab_id)
            .and_then(|runtime| runtime.shell.as_ref())
            .and_then(|view| view.more.clone().map(|more| (more, view.database.clone())))
        else {
            return;
        };
        let (more, database) = more;
        let same_session = self
            .workspace
            .tab(tab_id)
            .is_some_and(|tab| tab.connection_id == more.connection)
            && self.sessions.generation(&more.connection) == Some(more.epoch);
        if !same_session {
            self.fail_mongo_script(
                tab_id,
                "The connection was reopened, so the cursor is gone. Run the script again",
                cx,
            );
            return;
        }
        self.start_shell_run(tab_id, "it".into(), database, Some(more), false, cx);
    }

    /// Begins the run and hands it to a blocking thread.
    fn start_shell_run(
        &mut self,
        tab_id: u64,
        script: String,
        database: String,
        cursor: Option<MoreCursor>,
        history: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(connection) = self.workspace.tab(tab_id).map(|tab| tab.connection_id.clone())
        else {
            return;
        };
        let Some(page_size) = results::mongo::query::normalize_mongo_page(&mut self.workspace, tab_id)
        else {
            return;
        };
        let engine = Arc::clone(&self.mongo_shell.engine);
        // The commands of the script go through the tab's session; the engine of a test needs none.
        // Stop reaches the run through the host's cancel handle, not the tab's query operation.
        let session = if engine.needs_session() {
            let scheme = self
                .saved_connections
                .iter()
                .find(|saved| saved.matches_id(&connection))
                .map_or("mongodb", |saved| saved.kind.expected_scheme())
                .to_string();
            if let Some(capabilities) = self.driver_capabilities(&connection, &scheme) {
                if !capabilities.has(Capability::MongoCommand) {
                    let message =
                        self.outdated_driver_message("mongodb", "MongoDB", "cannot run shell scripts");
                    self.fail_mongo_script(tab_id, &message, cx);
                    return;
                }
            }
            match self.driver_runtime.sessions.get(&connection).cloned() {
                Some(session) => Some(session),
                None => {
                    self.fail_mongo_script(tab_id, "Driver session is not connected", cx);
                    return;
                }
            }
        } else {
            None
        };
        let Some(run) = self
            .workspace
            .runtime_mut(tab_id)
            .and_then(|runtime| runtime.query.begin())
        else {
            return;
        };
        if !self.query_tasks.registry.register(tab_id, run) {
            self.fail_mongo_script(tab_id, "Query task already exists", cx);
            return;
        }
        self.query_tasks.started_at.insert(tab_id, (run, Instant::now()));
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            if history {
                runtime.last_executed_query = Some(script.clone());
            }
            // What the last run showed does not stay beside a new one.
            runtime.shell = None;
        }
        let effective = self.effective_for(&connection);
        let request = ShellRequest {
            script: script.clone(),
            database: database.clone(),
            // Read-only connections run scripts read-only: the shell, the host and the driver
            // each refuse a write before it reaches the server.
            allow_writes: !effective.read_only,
            page_size: page_size.clamp(1, i64::from(u32::MAX)) as u32,
            timeout_ms: effective
                .query_timeout
                .map(|timeout| timeout.as_millis().min(u128::from(u64::MAX)) as u64),
            cursor: cursor.map(|more| more.cursor),
        };
        let slot = Arc::new(StopSlot::default());
        self.mongo_shell
            .runs
            .insert(tab_id, ShellRunHandle { run, slot: Arc::clone(&slot) });
        let job = ShellJob {
            engine,
            tools: self.driver_ui.tools.store.clone(),
            request,
            session,
            slot,
            generation: run.0,
        };
        let context = RunContext {
            tab_id,
            run,
            epoch: self.sessions.generation(&connection).unwrap_or(0),
            connection,
            script,
            database,
            history,
            page_size,
        };
        // `smol::unblock`: a script may run for minutes, which must not pin a worker of the
        // shared background pool.
        let work = cx
            .background_executor()
            .spawn(async move { smol::unblock(move || job.run()).await });
        let task = cx.spawn(async move |this, cx| {
            let outcome = work.await;
            let _ = this.update(cx, |this, cx| this.finish_shell_run(context, outcome, cx));
        });
        self.query_tasks.tasks.insert(tab_id, task);
        self.notify_tab(tab_id, cx);
    }

    /// The reply (or the failure) of a run arrived.
    fn finish_shell_run(
        &mut self,
        context: RunContext,
        outcome: Result<ReadyReply, ShellRunError>,
        cx: &mut Context<Self>,
    ) {
        let RunContext { tab_id, run, .. } = context;
        self.finish_query_timing(tab_id, run);
        if self.mongo_shell.runs.get(&tab_id).is_some_and(|handle| handle.run == run) {
            self.mongo_shell.runs.remove(&tab_id);
        }
        let live = self.workspace.runtime(tab_id).is_some_and(|runtime| {
            matches!(
                runtime.query.status(),
                crate::query::QueryStatus::Running { run: current } if *current == run
            )
        });
        // Stopped, closed or superseded while it ran: its reply is not shown.
        if !live {
            self.finish_query_task(tab_id, run);
            return;
        }
        match outcome {
            Err(error) if error.is_cancelled() => {
                if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
                    runtime.query.cancel();
                    runtime.shell = None;
                }
                self.finish_query_task(tab_id, run);
            }
            Err(error) => {
                // The plugin's own text (stderr, protocol errors) is untrusted: cleaned first, then
                // redacted, before the pane and the Error Log see it.
                let message = crate::sessions::redact_secrets(&crate::mongo_shell::clean_text(
                    &error.to_string(),
                ));
                if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
                    runtime.query.fail(run, message.clone());
                    runtime.shell = None;
                }
                self.finish_query_task(tab_id, run);
                self.report_query_failure(tab_id, &message);
                // Only the driver's own "not connected" says the session is gone; text from the
                // plugin (its stderr, protocol errors) never does.
                let lost = matches!(
                    &error,
                    ShellRunError::Host(ShellHostError::Driver(LoaderError::DriverRejected { code, .. }))
                        if crate::sessions::is_connection_lost_code(code)
                );
                if lost || self.tab_tunnel_is_down(tab_id) {
                    self.mark_tab_connection_lost(tab_id);
                }
            }
            Ok(ready) => self.apply_shell_reply(context, ready, cx),
        }
        self.notify_tab(tab_id, cx);
    }

    /// Shows a checked reply: the grid, the text, the Output, the error, the tab's database.
    fn apply_shell_reply(
        &mut self,
        context: RunContext,
        ready: ReadyReply,
        cx: &mut Context<Self>,
    ) {
        let RunContext { tab_id, run, .. } = context;
        let ReadyReply { checked, rows, page } = ready;
        let CheckedReply {
            output,
            output_truncated,
            value_text,
            value_lines,
            more,
            error,
            database_after,
            notes,
            documents: _,
        } = checked;
        let database = database_after.unwrap_or_else(|| context.database.clone());
        let failed_message = error.as_ref().map(|failure| {
            format!("{}: {}", failure_label(failure.kind), failure_text(failure))
        });
        // The grid is replaced; a driver page of an earlier find is let go first.
        self.release_driver_page(tab_id);
        // No documents: an empty grid with no columns.
        let (grid, columns) = match page {
            None => (
                GridState::new(GridContentSource::Arrow(ArrowGridSource::new(Vec::new()))),
                Vec::new(),
            ),
            Some((columns, lease)) => (
                GridState::new(GridContentSource::Bson(BsonGridSource::new(
                    lease,
                    columns.clone(),
                    rows as u64,
                ))),
                columns,
            ),
        };
        let view = ShellView {
            script: context.script.clone(),
            output,
            output_truncated,
            value_text,
            value_lines,
            documents: rows,
            more: more.map(|cursor| MoreCursor {
                cursor,
                connection: context.connection.clone(),
                epoch: context.epoch,
            }),
            error: error.clone(),
            database: database.clone(),
            notes,
            output_open: true,
        };
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            match &failed_message {
                Some(message) => {
                    runtime.query.fail(run, crate::sessions::redact_secrets(message));
                }
                None => {
                    runtime.query.complete(run, rows);
                }
            }
            runtime.mongo.reset_documents();
            runtime
                .mongo
                .observe_result_columns(columns.iter().map(|column| column.name.clone()));
            runtime.grid = grid;
            runtime.driver_page_id = None;
            runtime.pagination.reset();
            runtime
                .pagination
                .set_page_result_count(rows, Some(rows as u64), true);
            runtime.shell = Some(view);
        }
        if let Some(tab) = self.workspace.tab_mut(tab_id) {
            tab.database = database.clone();
        }
        self.finish_query_task(tab_id, run);
        if let Some(message) = &failed_message {
            self.report_query_failure(tab_id, message);
        }
        if context.history && failed_message.is_none() {
            let request = SqlRun {
                tab_id,
                kind: RunKind::Sql,
                connection: context.connection.clone(),
                sql: context.script.clone(),
                database,
                collection: self
                    .workspace
                    .tab(tab_id)
                    .map(|tab| tab.collection.clone())
                    .unwrap_or_default(),
                limit: context.page_size,
                offset: 0,
                confirmed: false,
                read_only: false,
            };
            self.record_query_history(&request, rows as i64, cx);
        }
        self.view_mut(tab_id)
            .grid_scroll
            .scroll_to_item_strict(0, gpui::ScrollStrategy::Top);
        self.reset_tab_view(tab_id);
        cx.notify();
    }

    /// The run of `tab_id` has a shell result (its Output, text or error) to show.
    pub(in crate::app) fn shell_view(&self, tab_id: u64) -> Option<&ShellView> {
        self.workspace.runtime(tab_id)?.shell.as_ref()
    }
}

#[cfg(all(test, feature = "bench"))]
impl AppRoot {
    /// Captures: shows `reply` as the result of `script` on `tab_id`, as if the plugin had
    /// answered (the reply goes through the same check and display as a real one).
    pub(in crate::app) fn show_shell_reply_for_capture(
        &mut self,
        tab_id: u64,
        script: &str,
        reply: engine_driver_api::mongo_shell::ShellReply,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.workspace.tab(tab_id) else {
            return;
        };
        let (connection, database) = (tab.connection_id.clone(), tab.database.clone());
        let Some(run) = self
            .workspace
            .runtime_mut(tab_id)
            .and_then(|runtime| runtime.query.begin())
        else {
            return;
        };
        let context = RunContext {
            tab_id,
            run,
            epoch: self.sessions.generation(&connection).unwrap_or(0),
            connection,
            script: script.to_string(),
            database: database.clone(),
            history: false,
            page_size: 200,
        };
        match prepare_reply(reply, &database, run.0) {
            Ok(ready) => self.apply_shell_reply(context, ready, cx),
            Err(error) => self.fail_mongo_script(tab_id, &error.to_string(), cx),
        }
    }
}
