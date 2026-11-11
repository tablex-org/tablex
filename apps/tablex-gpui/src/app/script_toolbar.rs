//! Toolbar of the Mongo New Query tab (`mongoScript`):
//! Run (all) / Run Selected (selection, else the statement at the cursor),
//! Format, Open / Save of `.js` files through the native file dialogs,
//! Export of the current result page, and the connection / database pickers
//! with "Set Default" (the sidebar's default-database preference).
//!
//! Controls are the official ones (`Button`, `DropdownMenu` via
//! `Button::dropdown_caret`, `Select`); the editor, its Save button (saved
//! script library, Cmd-S) and the run pipeline are the existing ones.
use super::*;
use gpui_component::select::{Select, SelectEvent, SelectState};

type TextSelect = SelectState<Vec<String>>;

struct ToolbarSelects {
    connection: gpui::Entity<TextSelect>,
    database: gpui::Entity<TextSelect>,
    /// `ScriptToolbarUi::connections_key` the connection Select was filled for.
    connections_key: u64,
    /// `(connection, database-list revision, tab database)` the database
    /// Select was filled for.
    databases_key: Option<(String, u64, String)>,
    _subscriptions: Vec<gpui::Subscription>,
}

/// Saved Mongo connections as the pickers show them, rebuilt only when the
/// saved connections change (`key`).
#[derive(Default)]
struct ConnectionChoices {
    key: u64,
    ids: Vec<String>,
    labels: Vec<String>,
}

#[derive(Default)]
pub(super) struct ScriptToolbarUi {
    tabs: HashMap<u64, ToolbarSelects>,
    choices: ConnectionChoices,
    /// Database names per connection (first page of the driver's root listing).
    databases: HashMap<String, Vec<String>>,
    /// Bumped whenever `databases` changes; Selects refill on a new value.
    databases_revision: u64,
    /// Last failed listing per connection: kept (not cached as an empty
    /// list) so a retry is possible, and never retried implicitly.
    database_errors: HashMap<String, String>,
    loading: HashSet<String>,
    /// One-line message per tab (file opened / saved / format result).
    notice: HashMap<u64, String>,
}

impl ScriptToolbarUi {
    pub(super) fn remove_tab(&mut self, tab_id: u64) {
        self.tabs.remove(&tab_id);
        self.notice.remove(&tab_id);
    }

    #[cfg(test)]
    pub(super) fn notice(&self, tab_id: u64) -> Option<&str> {
        self.notice.get(&tab_id).map(String::as_str)
    }

    #[cfg(test)]
    pub(super) fn database_select(&self, tab_id: u64) -> Option<&gpui::Entity<TextSelect>> {
        self.tabs.get(&tab_id).map(|selects| &selects.database)
    }

    #[cfg(test)]
    pub(super) fn connection_select(&self, tab_id: u64) -> Option<&gpui::Entity<TextSelect>> {
        self.tabs.get(&tab_id).map(|selects| &selects.connection)
    }

    #[cfg(test)]
    pub(super) fn database_error(&self, connection: &str) -> Option<&str> {
        self.database_errors.get(connection).map(String::as_str)
    }

    #[cfg(test)]
    pub(super) fn has_database_list(&self, connection: &str) -> bool {
        self.databases.contains_key(connection)
    }
}

/// Labels of the connection Select: connection names, disambiguated (with
/// their position) when two saved connections share a name. O(n).
fn connection_labels(names: &[&str]) -> Vec<String> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for name in names {
        *counts.entry(name).or_default() += 1;
    }
    names
        .iter()
        .enumerate()
        .map(|(index, name)| {
            if counts[name] > 1 {
                format!("{name} ({})", index + 1)
            } else {
                (*name).to_string()
            }
        })
        .collect()
}

impl AppRoot {
    /// Cheap fingerprint (no allocation) of what the connection Select shows.
    fn mongo_connections_key(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for saved in &self.saved_connections {
            if saved.kind == ConnectionKind::Mongodb && !saved.disabled {
                saved.id.hash(&mut hasher);
                saved.name.hash(&mut hasher);
            }
        }
        hasher.finish()
    }

    fn refresh_connection_choices(&mut self) {
        let key = self.mongo_connections_key();
        if self.script_toolbar.choices.key == key && !self.script_toolbar.choices.ids.is_empty() {
            return;
        }
        let mongo: Vec<&crate::connections::SavedConnection> = self
            .saved_connections
            .iter()
            .filter(|saved| saved.kind == ConnectionKind::Mongodb && !saved.disabled)
            .collect();
        let names: Vec<&str> = mongo.iter().map(|saved| saved.name.as_str()).collect();
        let choices = ConnectionChoices {
            key,
            ids: mongo.iter().map(|saved| saved.id.to_string()).collect(),
            labels: connection_labels(&names),
        };
        self.script_toolbar.choices = choices;
    }

    /// Database names for `connection`: the loaded listing plus the tab's own
    /// database (so the Select always shows what the tab targets).
    fn script_databases(&self, connection: &str, current: &str) -> Vec<String> {
        let mut names = self
            .script_toolbar
            .databases
            .get(connection)
            .cloned()
            .unwrap_or_default();
        if !current.is_empty() && !names.iter().any(|name| name == current) {
            names.insert(0, current.to_string());
        }
        names
    }

    /// Loads the database names of `connection` once (scenario fixture or the
    /// driver's root listing). A failure is remembered with its message and is
    /// retried only through [`Self::retry_script_databases`], never by itself.
    pub(super) fn load_script_databases(&mut self, connection: &str, cx: &mut Context<Self>) {
        if self.script_toolbar.databases.contains_key(connection)
            || self.script_toolbar.database_errors.contains_key(connection)
            || !self.script_toolbar.loading.insert(connection.to_string())
        {
            return;
        }
        let request = LoadRequest::Provider {
            connection: connection.to_string(),
            parent: None,
            payload: None,
            cursor: None,
            limit: navigator::PROVIDER_PAGE_SIZE,
        };
        let names_of = |page: &engine_driver_api::NavigatorPage| -> Vec<String> {
            page.nodes
                .iter()
                .filter(|node| node.kind == "database")
                .map(|node| node.label.clone())
                .collect()
        };
        if let Some(service) = self.scenario_ui.service.as_ref() {
            let result = match service.load(&request) {
                Ok(crate::ui_fixture::FixtureLoad::Provider(page)) => Ok(names_of(&page)),
                Ok(_) => Err("unexpected catalog answer".to_string()),
                Err(error) => Err(error),
            };
            self.finish_script_databases(connection, result);
            return;
        }
        let Some(session) = self.driver_runtime.sessions.get(connection).cloned() else {
            self.script_toolbar.loading.remove(connection);
            return;
        };
        let json = match serde_json::to_string(&navigator_ui::navigator_request_from_load(&request)) {
            Ok(json) => json,
            Err(error) => {
                self.finish_script_databases(connection, Err(error.to_string()));
                return;
            }
        };
        // A network call: on the blocking pool (never a worker of the shared background
        // pool), and without the session lock.
        let task = cx.background_executor().spawn(async move {
            smol::unblock(move || {
                session
                    .list_nav_nodes(&json)
                    .map_err(|error| error.to_string())
            })
            .await
        });
        let connection = connection.to_string();
        cx.spawn(async move |this, cx| {
            let result = task.await.map(|page| names_of(&page));
            let _ = this.update(cx, |this, cx| {
                this.finish_script_databases(&connection, result);
                cx.notify();
            });
        })
        .detach();
    }

    fn finish_script_databases(&mut self, connection: &str, result: Result<Vec<String>, String>) {
        if let Err(error) = &result {
            self.report_error(Area::Navigator, Some(connection), error);
        }
        let ui = &mut self.script_toolbar;
        ui.loading.remove(connection);
        match result {
            Ok(names) => {
                ui.database_errors.remove(connection);
                ui.databases.insert(connection.to_string(), names);
                ui.databases_revision += 1;
            }
            Err(error) => {
                ui.database_errors
                    .insert(connection.to_string(), crate::sessions::redact_secrets(&error));
                ui.databases_revision += 1;
            }
        }
    }

    /// Explicit retry after a failed listing.
    pub(super) fn retry_script_databases(&mut self, connection: &str, cx: &mut Context<Self>) {
        if self.script_toolbar.database_errors.remove(connection).is_some() {
            self.load_script_databases(connection, cx);
            cx.notify();
        }
    }

    /// Creates the pickers of a tab and keeps their items / selection in step
    /// with the saved connections, the loaded database list and the tab.
    /// Everything is compared by cheap keys: an unchanged toolbar does no
    /// allocation per frame beyond the two small key strings.
    fn sync_script_toolbar(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.workspace.tab(tab_id).is_none() {
            return;
        }
        self.refresh_connection_choices();
        if let std::collections::hash_map::Entry::Vacant(slot) = self.script_toolbar.tabs.entry(tab_id) {
            let conn_select = cx.new(|cx| TextSelect::new(Vec::new(), None, window, cx).searchable(true));
            let db_select = cx.new(|cx| TextSelect::new(Vec::new(), None, window, cx).searchable(true));
            let mut subscriptions = Vec::new();
            subscriptions.push(cx.subscribe_in(
                &conn_select,
                window,
                move |this: &mut Self,
                      _,
                      event: &SelectEvent<Vec<String>>,
                      _: &mut Window,
                      cx: &mut Context<Self>| {
                    if let SelectEvent::Confirm(Some(label)) = event {
                        this.choose_script_connection(tab_id, label, cx);
                    }
                },
            ));
            subscriptions.push(cx.subscribe_in(
                &db_select,
                window,
                move |this: &mut Self,
                      _,
                      event: &SelectEvent<Vec<String>>,
                      _: &mut Window,
                      cx: &mut Context<Self>| {
                    if let SelectEvent::Confirm(Some(name)) = event {
                        this.choose_script_database(tab_id, name.clone(), cx);
                    }
                },
            ));
            slot.insert(ToolbarSelects {
                connection: conn_select,
                database: db_select,
                connections_key: 0,
                databases_key: None,
                _subscriptions: subscriptions,
            });
        }
        let (connection, database) = match self.workspace.tab(tab_id) {
            Some(tab) => (tab.connection_id.clone(), tab.database.clone()),
            None => return,
        };
        let databases_key = (
            connection.clone(),
            self.script_toolbar.databases_revision,
            database.clone(),
        );
        let refill_databases = self
            .script_toolbar
            .tabs
            .get(&tab_id)
            .is_some_and(|selects| selects.databases_key.as_ref() != Some(&databases_key));
        let databases = refill_databases.then(|| self.script_databases(&connection, &database));
        let choices_key = self.script_toolbar.choices.key;
        let Some(selects) = self.script_toolbar.tabs.get_mut(&tab_id) else {
            return;
        };
        if selects.connections_key != choices_key {
            let items = self.script_toolbar.choices.labels.clone();
            selects
                .connection
                .update(cx, |state, cx| state.set_items(items, window, cx));
            selects.connections_key = choices_key;
        }
        if let Some(databases) = databases {
            selects
                .database
                .update(cx, |state, cx| state.set_items(databases, window, cx));
            selects.databases_key = Some(databases_key);
        }
        // Selection follows the tab.
        let choices = &self.script_toolbar.choices;
        if let Some(position) = choices
            .ids
            .iter()
            .position(|id| id.eq_ignore_ascii_case(&connection))
        {
            let wanted = &choices.labels[position];
            if selects.connection.read(cx).selected_value() != Some(wanted) {
                let wanted = wanted.clone();
                selects
                    .connection
                    .update(cx, |state, cx| state.set_selected_value(&wanted, window, cx));
            }
        }
        if !database.is_empty() && selects.database.read(cx).selected_value() != Some(&database) {
            selects
                .database
                .update(cx, |state, cx| state.set_selected_value(&database, window, cx));
        }
    }

    /// Moves a script tab to another connection. The tab's result belongs to
    /// the old connection, so first: stop its in-flight query, hand its driver
    /// page back **through the old connection** (the page cache is per
    /// connection; sending its id to another one would be refused and leak
    /// the page until disconnect), and clear the result and paging.
    pub(super) fn retarget_script_connection(
        &mut self,
        tab_id: u64,
        connection: &str,
        cx: &mut Context<Self>,
    ) {
        let default = self.mongo_default_database(connection).unwrap_or_default();
        let unchanged = self
            .workspace
            .tab(tab_id)
            .is_none_or(|tab| tab.connection_id.eq_ignore_ascii_case(connection));
        if unchanged {
            return;
        }
        // A tab with a transaction open cannot just be moved: its session belongs to the old
        // connection. Ask first, then drop the session and the tab's transaction state.
        let target = connection.to_owned();
        if self.guard_tabs_deferred(vec![tab_id], "Moving the tab to another connection ends its session.", cx, move |this, _, cx| {
            this.retarget_script_connection(tab_id, &target, cx)
        }) {
            return;
        }
        self.cancel_tab_query(tab_id, cx);
        self.release_driver_page(tab_id);
        self.tab_transaction_closed(tab_id);
        if let Some(tab) = self.workspace.tab_mut(tab_id) {
            tab.connection_id = connection.to_string();
            // The database belongs to the connection: start from its default.
            tab.database = default;
        }
        self.reset_script_result(tab_id);
        self.load_script_databases(connection, cx);
        self.reset_tab_view(tab_id);
        cx.notify();
    }

    /// Forgets the displayed result and paging of a tab (the target changed).
    fn reset_script_result(&mut self, tab_id: u64) {
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.pagination = crate::query::PaginationState::new(runtime.pagination.page_size());
            runtime.grid = GridState::new(GridContentSource::Arrow(ArrowGridSource::new(Vec::new())));
            runtime.mongo.reset_documents();
            runtime.query = crate::query::QueryState::default();
            runtime.last_executed_query = None;
            runtime.shell = None;
        }
    }

    fn choose_script_connection(&mut self, tab_id: u64, label: &str, cx: &mut Context<Self>) {
        self.refresh_connection_choices();
        let choices = &self.script_toolbar.choices;
        let Some(position) = choices.labels.iter().position(|candidate| candidate == label) else {
            return;
        };
        let connection = choices.ids[position].clone();
        self.retarget_script_connection(tab_id, &connection, cx);
    }

    /// A database switch stays on the same connection, so its page remains
    /// valid for replacement; only the paging offset and the shown result
    /// belong to the old target.
    pub(super) fn choose_script_database(&mut self, tab_id: u64, database: String, cx: &mut Context<Self>) {
        if self
            .workspace
            .tab(tab_id)
            .is_none_or(|tab| tab.database == database)
        {
            return;
        }
        if let Some(tab) = self.workspace.tab_mut(tab_id) {
            tab.database = database;
        }
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.pagination.reset();
            // A shell result belongs to the database it ran in (and its cursor to that run).
            runtime.shell = None;
        }
        cx.notify();
    }

    // ---- actions ------------------------------------------------------------------

    /// Run Selected: the selection, else the statement at the cursor.
    pub(super) fn run_script_selection(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let Some(editor) = self.query_editors.by_tab.get(&tab_id).cloned() else {
            return;
        };
        let (selection, text, cursor) = editor.read_with(cx, |state, _| {
            (
                state.selected_value().to_string(),
                state.value().to_string(),
                state.cursor(),
            )
        });
        let script = if selection.trim().is_empty() {
            crate::mongo_script::statement_at(&text, cursor).map(|range| text[range].to_string())
        } else {
            Some(selection)
        };
        match script {
            Some(script) if !script.trim().is_empty() => self.run_mongo_script_text(tab_id, script, cx),
            _ => {
                self.script_toolbar
                    .notice
                    .insert(tab_id, "Nothing to run: select text or put the cursor in a statement.".into());
                cx.notify();
            }
        }
    }

    /// Replaces the script of a tab: the editor shows it and the tab (which
    /// persists in the session and is what Run reads) holds it. A programmatic
    /// `set_value` raises no change event, so both are written here.
    pub(super) fn set_script_text(
        &mut self,
        tab_id: u64,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(editor) = self.query_editors.by_tab.get(&tab_id).cloned() {
            let shown = text.clone();
            editor.update(cx, |state, cx| state.set_value(shown, window, cx));
        }
        if let Some(tab) = self.workspace.tab_mut(tab_id) {
            tab.sql = text;
        }
    }

    /// Re-indents the script (object / array arguments), keeping key order.
    pub(super) fn format_script_tab(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(editor) = self.query_editors.by_tab.get(&tab_id).cloned() else {
            return;
        };
        let text = editor.read(cx).value().to_string();
        let notice = match crate::mongo_script::format_script(&text) {
            Ok(formatted) if formatted == text => "Already formatted".to_string(),
            Ok(formatted) => {
                self.set_script_text(tab_id, formatted, window, cx);
                "Formatted".to_string()
            }
            Err(error) => format!("Cannot format: {error}"),
        };
        self.script_toolbar.notice.insert(tab_id, notice);
        cx.notify();
    }

    /// Open a `.js` file into the editor (native file dialog).
    pub(super) fn open_script_file(&mut self, tab_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let prompt = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Open MongoDB script".into()),
        });
        let handle = window.window_handle();
        cx.spawn(async move |this, cx| {
            let path = match prompt.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                Ok(Ok(None)) => return,
                _ => {
                    let _ = this.update(cx, |this, cx| {
                        this.script_toolbar
                            .notice
                            .insert(tab_id, "Could not open the file picker".into());
                        cx.notify();
                    });
                    return;
                }
            };
            let Some(path) = path else { return };
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            let read_path = path.clone();
            let text = cx
                .background_executor()
                .spawn(async move {
                    smol::unblock(move || -> Result<String, String> {
                        use std::io::Read as _;
                        let mut data = Vec::new();
                        std::fs::File::open(&read_path)
                            .and_then(|file| file.take(2_000_001).read_to_end(&mut data))
                            .map_err(|error| format!("Could not read {}: {error}", read_path.display()))?;
                        if data.len() > 2_000_000 {
                            return Err("The file is larger than 2 MB.".into());
                        }
                        String::from_utf8(data).map_err(|_| "The file is not UTF-8 text.".to_string())
                    })
                    .await
                })
                .await;
            let _ = handle.update(cx, |_, window, cx| {
                let _ = this.update(cx, |this, cx| match text {
                    Ok(text) => {
                        this.set_script_text(tab_id, text, window, cx);
                        this.script_toolbar
                            .notice
                            .insert(tab_id, format!("Opened {name}"));
                        cx.notify();
                    }
                    Err(error) => {
                        this.report_error(Area::App, None, &error);
                        this.script_toolbar.notice.insert(tab_id, error);
                        cx.notify();
                    }
                });
            });
        })
        .detach();
    }

    /// Save the editor text as a `.js` file (native save dialog).
    pub(super) fn save_script_file(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let Some(editor) = self.query_editors.by_tab.get(&tab_id).cloned() else {
            return;
        };
        let text = editor.read(cx).value().to_string();
        let directory = crate::platform::save_dialog_dir();
        let suggested = self
            .workspace
            .tab(tab_id)
            .map(|tab| {
                let base: String = tab
                    .collection
                    .chars()
                    .map(|ch| if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' { ch } else { '_' })
                    .collect();
                if base.is_empty() { "script.js".to_string() } else { format!("{base}.js") }
            })
            .unwrap_or_else(|| "script.js".into());
        let prompt = cx.prompt_for_new_path(&directory, Some(&suggested));
        cx.spawn(async move |this, cx| {
            let path = match prompt.await {
                Ok(Ok(Some(path))) => path,
                Ok(Ok(None)) => return,
                _ => {
                    let _ = this.update(cx, |this, cx| {
                        this.script_toolbar
                            .notice
                            .insert(tab_id, "Could not open the save dialog".into());
                        cx.notify();
                    });
                    return;
                }
            };
            let shown = path.display().to_string();
            let write_path = path.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    smol::unblock(move || std::fs::write(&write_path, text.as_bytes())).await
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Err(error) = &result {
                    this.report_error(Area::Export, None, format!("Could not save {shown}: {error}"));
                }
                this.script_toolbar.notice.insert(
                    tab_id,
                    match result {
                        Ok(()) => format!("Saved {shown}"),
                        Err(error) => format!("Could not save {shown}: {error}"),
                    },
                );
                cx.notify();
            });
        })
        .detach();
    }

    /// Set (or clear) the connection's default database from the tab's.
    fn toggle_script_default_database(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let Some((connection, database)) = self
            .workspace
            .tab(tab_id)
            .map(|tab| (tab.connection_id.clone(), tab.database.clone()))
        else {
            return;
        };
        if database.is_empty() || connection.is_empty() {
            self.script_toolbar
                .notice
                .insert(tab_id, "Select a database first.".into());
            cx.notify();
            return;
        }
        let is_default = self.mongo_default_database(&connection).as_deref() == Some(database.as_str());
        let next = (!is_default).then_some(database.as_str());
        if let Err(error) = self.set_mongo_default_database(&connection, next, cx) {
            self.script_toolbar
                .notice
                .insert(tab_id, format!("Could not save default database: {error}"));
        }
        cx.notify();
    }

    // ---- rendering ----------------------------------------------------------------

    /// The header row above the script editor.
    pub(super) fn render_mongo_script_toolbar(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.sync_script_toolbar(tab_id, window, cx);
        let Some((connection, database)) = self
            .workspace
            .tab(tab_id)
            .map(|tab| (tab.connection_id.clone(), tab.database.clone()))
        else {
            return div().into_any_element();
        };
        if !connection.is_empty() {
            self.load_script_databases(&connection, cx);
        }
        let running = self
            .workspace
            .runtime(tab_id)
            .is_some_and(|runtime| !runtime.query.can_run());
        let database_error = self.script_toolbar.database_errors.get(&connection).cloned();
        let notice = self
            .script_toolbar
            .notice
            .get(&tab_id)
            .cloned()
            .or_else(|| {
                database_error
                    .as_ref()
                    .map(|error| format!("Could not list databases ({error}). Click the database picker to retry."))
            });
        let retry_connection = database_error.is_some().then(|| connection.clone());
        let is_default = !database.is_empty()
            && self.mongo_default_database(&connection).as_deref() == Some(database.as_str());
        let (conn_select, db_select) = match self.script_toolbar.tabs.get(&tab_id) {
            Some(selects) => (selects.connection.clone(), selects.database.clone()),
            None => return div().into_any_element(),
        };
        let root = cx.entity().downgrade();

        let run = Button::new(("script-run", tab_id))
            .small()
            .debug_selector(|| "run-query".into())
            .icon(Icon::empty().path("icons/play.svg").size(px(12.0)))
            .label(if running { "Cancel" } else { "Run" })
            .compact()
            .primary()
            .on_click(cx.listener(move |this, _, _, cx| {
                if this
                    .workspace
                    .runtime(tab_id)
                    .is_some_and(|runtime| !runtime.query.can_run())
                {
                    this.cancel_active_tab_query(cx);
                } else {
                    this.run_active_tab_query(cx);
                }
            }));
        let run_selected = Button::new(("script-run-selected", tab_id))
            .small()
            .debug_selector(|| "script-run-selected".into())
            .label("Run Selected")
            .compact()
            .ghost()
            .tooltip("Run the selection, or the statement at the cursor")
            .disabled(running)
            .on_click(cx.listener(move |this, _, _, cx| this.run_script_selection(tab_id, cx)));
        let format = Button::new(("script-format", tab_id))
            .small()
            .debug_selector(|| "script-format".into())
            .icon(Icon::empty().path("icons/wand-sparkles.svg").size(px(12.0)))
            .label("Format")
            .compact()
            .ghost()
            .on_click(cx.listener(move |this, _, window, cx| this.format_script_tab(tab_id, window, cx)));
        let open = Button::new(("script-open", tab_id))
            .small()
            .debug_selector(|| "script-open".into())
            .icon(Icon::new(IconName::FolderOpen).size(px(12.0)))
            .compact()
            .ghost()
            .tooltip("Open a .js file")
            .on_click(cx.listener(move |this, _, window, cx| this.open_script_file(tab_id, window, cx)));
        let save_file = Button::new(("script-save-file", tab_id))
            .small()
            .debug_selector(|| "script-save-file".into())
            .icon(Icon::empty().path("icons/download.svg").size(px(12.0)))
            .compact()
            .ghost()
            .tooltip("Save the script as a .js file")
            .on_click(cx.listener(move |this, _, _, cx| this.save_script_file(tab_id, cx)));
        let export_root = root.clone();
        let rows = self.grid(tab_id).row_count();
        let export = Button::new(("script-export", tab_id))
            .small()
            .debug_selector(|| "script-export".into())
            .label("Export")
            .dropdown_caret(true)
            .compact()
            .ghost()
            .disabled(rows == 0)
            .dropdown_menu(move |menu, _, _| {
                results::export::ContextExportFormat::ALL
                    .iter()
                    .filter(|(format, _)| *format != results::export::ContextExportFormat::Sql)
                    .fold(menu, |menu, (format, label)| {
                        let root = export_root.clone();
                        let format = *format;
                        let selector = format!("script-export-{}", label.to_lowercase().replace(' ', "-"));
                        menu.item(
                            PopupMenuItem::element(move |_, _| {
                                let selector = selector.clone();
                                div()
                                    .debug_selector(move || selector.clone())
                                    .child(format!("Current Page {label}"))
                            })
                            .on_click(move |_, window, app| {
                                let _ = root.update(app, |this, cx| {
                                    this.export_context_selection(tab_id, format, true, window, cx)
                                });
                            }),
                        )
                    })
            });
        let save = Button::new("save-sql-script")
            .small()
            .debug_selector(|| "save-sql-script".into())
            .label("Save")
            .compact()
            .ghost()
            .tooltip(shortcuts::with_label(
                "Save Mongo script to the library",
                &self.settings.shortcuts,
                "editor.save_script",
            ))
            .on_click(cx.listener(|this, _, window, cx| this.open_save_sql_dialog(window, cx)));
        let set_default = Button::new(("script-set-default", tab_id))
            .small()
            .debug_selector(|| "script-set-default".into())
            // Icon-only so the toolbar stays one row at the default sidebar width.
            .icon(Icon::empty().path("icons/pin.svg").size(px(12.0)))
            .compact()
            .ghost()
            .selected(is_default)
            .tooltip(if is_default {
                "Default database for this connection (click to clear)"
            } else {
                "Set as default database for this connection"
            })
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_script_default_database(tab_id, cx)));
        let separator = || div().w(px(1.0)).h(px(16.0)).bg(theme::border());
        div()
            .debug_selector(|| "mongo-script-toolbar".into())
            .flex()
            .flex_col()
            .bg(theme::chrome())
            .border_b_1()
            .border_color(theme::border())
            .child(
                // One row: the action group keeps its size and the two pickers
                // shrink (down to a readable minimum) instead of wrapping.
                div()
                    .min_h(px(theme::HEADER_HEIGHT + 6.0))
                    .px(px(8.0))
                    .py(px(2.0))
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .overflow_hidden()
                    .child(
                        div()
                            .flex()
                            .flex_shrink_0()
                            .items_center()
                            .gap(px(4.0))
                            .child(run)
                            .child(run_selected)
                            .child(separator())
                            .child(format)
                            .child(open)
                            .child(save_file)
                            .child(save)
                            .child(separator())
                            .child(export),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .debug_selector(|| "script-connection".into())
                            .w(px(150.0))
                            .min_w(px(96.0))
                            .flex_shrink(1.0)
                            .child(
                                Select::new(&conn_select)
                                    .id(("script-connection-select", tab_id))
                                    .with_size(Size::Small)
                                    .placeholder("Connection")
                                    .search_placeholder("Search connections")
                                    .menu_width(px(240.0)),
                            ),
                    )
                    .child(
                        div()
                            .debug_selector(|| "script-database".into())
                            .w(px(150.0))
                            .min_w(px(96.0))
                            .flex_shrink(1.0)
                            .when_some(retry_connection, |this, connection| {
                                // A failed listing is retried by the user's own click.
                                this.on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _, _, cx| {
                                        this.retry_script_databases(&connection, cx)
                                    }),
                                )
                            })
                            .child(
                                Select::new(&db_select)
                                    .id(("script-database-select", tab_id))
                                    .with_size(Size::Small)
                                    .placeholder("Database")
                                    .search_placeholder("Search databases")
                                    .menu_width(px(240.0)),
                            ),
                    )
                    .child(div().flex_shrink_0().child(set_default)),
            )
            .when_some(notice, |this, notice| {
                this.child(
                    div()
                        .debug_selector(|| "script-notice".into())
                        .px(px(12.0))
                        .pb(px(4.0))
                        .text_size(px(11.0))
                        .text_color(theme::text_muted())
                        .child(notice),
                )
            })
            .into_any_element()
    }
}
