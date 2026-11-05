use super::Area;
use crate::connections::{self, ConnectionKind, SavedConnection};
use crate::credentials::{CredentialStore, SecretKind, SystemStore};
use crate::query_mode::{QueryMode, QueryModeExt};
use engine_driver_api::Capability;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

const MAX_FILTER_NAMES: usize = 100_000;

#[cfg(test)]
pub struct VisibleFilterDraft {
    pub available: Vec<String>,
    pub search: String,
    pub filter: crate::sidebar_preferences::VisibleObjectFilter,
}

#[cfg(test)]
impl VisibleFilterDraft {
    pub fn new(
        available: Vec<String>,
        filter: crate::sidebar_preferences::VisibleObjectFilter,
    ) -> Self {
        Self {
            available,
            search: String::new(),
            filter,
        }
    }
    pub fn displayed(&self) -> Vec<&str> {
        let query = self.search.to_lowercase();
        self.available
            .iter()
            .filter(|name| {
                self.filter.show_system || !crate::sidebar_preferences::is_system_database(name)
            })
            .filter(|name| name.to_lowercase().contains(&query))
            .map(String::as_str)
            .collect()
    }
    pub fn select_filtered(&mut self) {
        self.filter.databases = Some(self.displayed().into_iter().map(str::to_string).collect());
        self.filter.patterns.clear();
    }
    pub fn select_all(&mut self) {
        self.filter.databases = Some(
            self.available
                .iter()
                .filter(|name| {
                    self.filter.show_system || !crate::sidebar_preferences::is_system_database(name)
                })
                .cloned()
                .collect(),
        );
        self.filter.patterns.clear();
    }
    pub fn clear(&mut self) {
        self.filter.databases = Some(Vec::new());
        self.filter.patterns.clear();
    }
    pub fn show_all(&mut self) {
        self.filter = crate::sidebar_preferences::VisibleObjectFilter::default();
    }
}

pub fn enumerate_provider_databases(
    cancelled: &AtomicBool,
    mut fetch: impl FnMut(
        engine_driver_api::NavigatorRequest,
    ) -> Result<engine_driver_api::NavigatorPage, String>,
) -> Result<Vec<String>, String> {
    let mut names = Vec::new();
    let mut seen_names = HashSet::new();
    let mut seen_cursors = HashSet::new();
    let mut cursor = None;
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Err("Enumeration cancelled".into());
        }
        let page = fetch(engine_driver_api::NavigatorRequest {
            parent: None,
            database: None,
            payload: None,
            cursor,
            limit: 200,
        })?;
        for node in page.nodes {
            if node.kind != "database" {
                continue;
            }
            let name = node
                .payload
                .get("database")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(&node.label);
            if seen_names.insert(name.to_string()) {
                if names.len() >= MAX_FILTER_NAMES {
                    return Err("Database list exceeds the supported limit".into());
                }
                names.push(name.to_string());
            }
        }
        match page.next_cursor {
            Some(next) if seen_cursors.insert(next.clone()) => cursor = Some(next),
            Some(_) => return Err("Database list repeated a page cursor".into()),
            None => return Ok(names),
        }
    }
}

#[derive(Default)]
pub struct FilterEnumerationState {
    next: u64,
    pending: HashMap<String, (u64, u64, Arc<AtomicBool>)>,
    names: HashMap<String, Vec<String>>,
    errors: HashMap<String, String>,
    completed_epochs: HashMap<String, u64>,
    schema_pending: HashMap<(String, String), (u64, u64)>,
    schema_names: HashMap<(String, String), Vec<String>>,
    schema_errors: HashMap<(String, String), String>,
}

#[derive(Default)]
pub struct SavedSqlUiState {
    projects_base: Option<std::path::PathBuf>,
    include_registered: bool,
    next: u64,
    pending: HashMap<String, (u64, u64)>,
    entries: HashMap<String, Vec<crate::saved_sql::SavedSqlEntry>>,
    errors: HashMap<String, String>,
}

impl SavedSqlUiState {
    pub fn source(&self) -> Result<(std::path::PathBuf, bool), String> {
        Ok((
            self.projects_base
                .clone()
                .ok_or("SQL projects path unavailable")?,
            self.include_registered,
        ))
    }

    pub fn set_source(&mut self, base: std::path::PathBuf, include_registered: bool) {
        self.projects_base = Some(base);
        self.include_registered = include_registered;
    }
    pub fn begin(&mut self, connection: &str, epoch: u64) -> u64 {
        self.next = self.next.wrapping_add(1);
        self.pending.insert(connection.into(), (self.next, epoch));
        self.entries.remove(connection);
        self.errors.remove(connection);
        self.next
    }
    pub fn cancel(&mut self, connection: &str) {
        self.pending.remove(connection);
    }
    pub fn cancel_if(&mut self, connection: &str, token: u64) {
        if self
            .pending
            .get(connection)
            .is_some_and(|(current, _)| *current == token)
        {
            self.cancel(connection);
        }
    }
    pub fn loading(&self, connection: &str) -> bool {
        self.pending.contains_key(connection)
    }
    pub fn entries(&self, connection: &str) -> Option<&[crate::saved_sql::SavedSqlEntry]> {
        self.entries.get(connection).map(Vec::as_slice)
    }
    pub fn error(&self, connection: &str) -> Option<&str> {
        self.errors.get(connection).map(String::as_str)
    }
    pub fn finish(
        &mut self,
        connection: &str,
        token: u64,
        epoch: u64,
        result: Result<Vec<crate::saved_sql::SavedSqlEntry>, String>,
    ) -> bool {
        if self.pending.get(connection).copied() != Some((token, epoch)) {
            return false;
        }
        self.pending.remove(connection);
        match result {
            Ok(entries) => {
                self.entries.insert(connection.into(), entries);
                self.errors.remove(connection);
            }
            Err(error) => {
                self.entries.remove(connection);
                self.errors.insert(connection.into(), error);
            }
        }
        true
    }
}

impl FilterEnumerationState {
    pub fn begin(&mut self, connection: &str, epoch: u64) -> (u64, Arc<AtomicBool>) {
        self.cancel(connection);
        self.next = self.next.wrapping_add(1);
        let flag = Arc::new(AtomicBool::new(false));
        self.pending
            .insert(connection.into(), (self.next, epoch, flag.clone()));
        self.names.remove(connection);
        self.errors.remove(connection);
        self.completed_epochs.remove(connection);
        (self.next, flag)
    }

    pub fn cancel(&mut self, connection: &str) {
        if let Some((_, _, flag)) = self.pending.remove(connection) {
            flag.store(true, Ordering::Release);
        }
        self.schema_pending.retain(|(id, _), _| id != connection);
        self.schema_names.retain(|(id, _), _| id != connection);
        self.schema_errors.retain(|(id, _), _| id != connection);
        self.names.remove(connection);
        self.errors.remove(connection);
        self.completed_epochs.remove(connection);
    }
    pub fn cancel_if(&mut self, connection: &str, token: u64) {
        if self
            .pending
            .get(connection)
            .is_some_and(|(current, _, _)| *current == token)
        {
            self.cancel(connection);
        }
    }

    pub fn loading(&self, connection: &str) -> bool {
        self.pending.contains_key(connection)
    }
    pub fn names(&self, connection: &str) -> Option<&[String]> {
        self.names.get(connection).map(Vec::as_slice)
    }
    pub fn error(&self, connection: &str) -> Option<&str> {
        self.errors.get(connection).map(String::as_str)
    }
    pub fn completed_epoch(&self, connection: &str) -> Option<u64> {
        self.completed_epochs.get(connection).copied()
    }
    pub fn begin_schema(&mut self, connection: &str, database: &str, epoch: u64) -> u64 {
        self.next = self.next.wrapping_add(1);
        let key = (connection.to_string(), database.to_string());
        self.schema_names.remove(&key);
        self.schema_errors.remove(&key);
        self.schema_pending.insert(key, (self.next, epoch));
        self.next
    }
    pub fn schema_names(&self, connection: &str, database: &str) -> Option<&[String]> {
        self.schema_names
            .get(&(connection.into(), database.into()))
            .map(Vec::as_slice)
    }
    pub fn schema_error(&self, connection: &str, database: &str) -> Option<&str> {
        self.schema_errors
            .get(&(connection.into(), database.into()))
            .map(String::as_str)
    }
    pub fn schema_loading(&self, connection: &str, database: &str) -> bool {
        self.schema_pending
            .contains_key(&(connection.into(), database.into()))
    }
    pub fn cancel_schema_if(&mut self, connection: &str, database: &str, token: u64) {
        let key = (connection.to_string(), database.to_string());
        if self
            .schema_pending
            .get(&key)
            .is_some_and(|(current, _)| *current == token)
        {
            self.schema_pending.remove(&key);
            self.schema_names.remove(&key);
            self.schema_errors.remove(&key);
        }
    }
    pub fn finish_schema(
        &mut self,
        connection: &str,
        database: &str,
        token: u64,
        epoch: u64,
        result: Result<Vec<String>, String>,
    ) -> bool {
        let key = (connection.to_string(), database.to_string());
        if self.schema_pending.get(&key).copied() != Some((token, epoch)) {
            return false;
        }
        self.schema_pending.remove(&key);
        match result {
            Ok(names) => {
                self.schema_names.insert(key.clone(), names);
                self.schema_errors.remove(&key);
            }
            Err(error) => {
                self.schema_names.remove(&key);
                self.schema_errors.insert(key, error);
            }
        }
        true
    }

    pub fn finish(
        &mut self,
        connection: &str,
        token: u64,
        epoch: u64,
        result: Result<Vec<String>, String>,
    ) -> bool {
        if !self
            .pending
            .get(connection)
            .is_some_and(|(run, expected, flag)| {
                *run == token && *expected == epoch && !flag.load(Ordering::Acquire)
            })
        {
            return false;
        }
        self.pending.remove(connection);
        match result {
            Ok(names) => {
                self.names.insert(connection.into(), names);
                self.errors.remove(connection);
                self.completed_epochs.insert(connection.into(), epoch);
            }
            Err(error) => {
                self.names.remove(connection);
                self.errors.insert(connection.into(), error);
            }
        }
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyInfo {
    ConnectionUrl,
    ConnectionUrlWithPassword,
    HostPort,
}

/// Produces text for one explicitly selected clipboard action. The caller alone
/// writes the clipboard; failures leave its previous contents untouched.
pub fn copy_info(
    conn: &SavedConnection,
    item: CopyInfo,
    mut load_password: impl FnMut(&str) -> Option<String>,
) -> Result<String, String> {
    connections::validate_uri(conn.kind, &conn.uri)?;
    if item == CopyInfo::HostPort {
        return host_port(conn.kind, &conn.uri);
    }
    if conn.kind != ConnectionKind::Sqlite {
        host_port(conn.kind, &conn.uri)?;
    }
    // Reuse the export allowlist: unknown query keys can contain proxy tokens.
    let safe = connections::public_connection_uri(&conn.uri)?;
    if item == CopyInfo::ConnectionUrl || conn.kind == ConnectionKind::Sqlite {
        return Ok(safe);
    }
    if !conn.save_password {
        return Err("No saved password is available for this connection".into());
    }
    let password = load_password(&conn.id.to_string().to_uppercase())
        .filter(|secret| !secret.is_empty())
        .ok_or_else(connections::missing_password_error)?;
    // Explicit sensitive action uses the complete persisted URL; applying the
    // public export allowlist here would silently discard required driver options.
    let complete = connections::sanitized_uri(&conn.uri)?;
    connections::runtime_uri(&complete, &password)
}

pub fn copy_saved_connection_info(
    conn: &SavedConnection,
    item: CopyInfo,
) -> Result<String, String> {
    copy_info(conn, item, |account| {
        let store = SystemStore::new();
        let load = |account: &str| store.load(SecretKind::Database, account).ok().flatten();
        load(account).or_else(|| load(&account.to_lowercase()))
    })
}

fn host_port(kind: ConnectionKind, uri: &str) -> Result<String, String> {
    if kind == ConnectionKind::Sqlite {
        return Err("SQLite connections have no host or port".into());
    }
    let authority = uri
        .split_once("://")
        .map(|(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or_default())
        .unwrap_or_default();
    let hosts = authority
        .rsplit_once('@')
        .map_or(authority, |(_, hosts)| hosts);
    if uri
        .get(..14)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("mongodb+srv://"))
        && hosts.contains(',')
    {
        return Err("SRV connections require a single hostname".into());
    }
    let mut result = Vec::new();
    for host in hosts.split(',') {
        let parsed = url::Url::parse(&format!("{}://{host}/", kind.expected_scheme()))
            .map_err(|_| "Invalid connection URI")?;
        let hostname = parsed.host().ok_or("Invalid connection URI")?;
        if parsed.username() != "" || parsed.password().is_some() || parsed.path() != "/" {
            return Err("Invalid connection URI".into());
        }
        if kind == ConnectionKind::Mongodb
            && uri.starts_with("mongodb+srv://")
            && parsed.port().is_some()
        {
            return Err("SRV connections cannot specify a port".into());
        }
        let address = match parsed.port() {
            Some(port) => format!("{hostname}:{port}"),
            None => hostname.to_string(),
        };
        result.push(address);
    }
    if result.is_empty() {
        return Err("Invalid connection URI".into());
    }
    Ok(result.join(","))
}

#[cfg(test)]
pub fn save_active_query(
    workspace: &crate::workspace::WorkspaceState,
    project_root: &std::path::Path,
    name: &str,
    now: f64,
) -> Result<crate::saved_sql::SavedSqlEntry, String> {
    let tab = workspace.active_tab().ok_or("No active query")?;
    if tab.kind != crate::workspace::TabKind::Query
        || tab.connection_id.is_empty()
        || tab.query_mode.is_mongo_find()
    {
        return Err("Only a connected SQL query can be saved".into());
    }
    crate::saved_sql::save_sql(
        project_root,
        &tab.connection_id,
        &tab.database,
        name,
        &tab.sql,
        now,
    )
}

#[cfg(test)]
pub fn open_saved_query(
    workspace: &mut crate::workspace::WorkspaceState,
    entry: &crate::saved_sql::SavedSqlEntry,
    connection_id: &str,
) -> Result<u64, String> {
    if !uuid::Uuid::parse_str(connection_id).is_ok_and(|id| id.to_string() == entry.connection_id) {
        return Err("Saved SQL belongs to a different connection".into());
    }
    let sql = crate::saved_sql::open_sql(entry)?;
    open_saved_query_with_contents(workspace, entry, connection_id, sql)
}

fn open_saved_query_with_contents(
    workspace: &mut crate::workspace::WorkspaceState,
    entry: &crate::saved_sql::SavedSqlEntry,
    connection_id: &str,
    sql: String,
) -> Result<u64, String> {
    if !uuid::Uuid::parse_str(connection_id).is_ok_and(|id| id.to_string() == entry.connection_id) {
        return Err("Saved SQL belongs to a different connection".into());
    }
    let id = workspace.open_query_tab_with_metadata(
        &entry.title,
        crate::workspace::TabMetadata {
            connection_id: entry.connection_id.clone(),
            database: entry.database.clone(),
            schema: entry.schema.clone(),
            query_mode: (entry.dialect == QueryMode::MongoScript.as_str())
                .then_some(QueryMode::MongoScript),
            ..Default::default()
        },
    );
    if let Some(tab) = workspace.active_tab_mut() {
        tab.sql = sql;
    }
    Ok(id)
}

impl super::AppRoot {
    pub(super) fn visible_filter_schema_state(
        &self,
        connection_id: &str,
        database: &str,
    ) -> (bool, Option<&[String]>, Option<&str>) {
        (
            self.filter_enumeration
                .schema_loading(connection_id, database),
            self.filter_enumeration
                .schema_names(connection_id, database),
            self.filter_enumeration
                .schema_error(connection_id, database),
        )
    }

    pub(super) fn request_visible_filter_schemas(
        &mut self,
        connection_id: &str,
        database: &str,
        cx: &mut gpui::Context<Self>,
    ) -> Result<(), String> {
        let saved = self
            .saved_connections
            .iter()
            .find(|saved| saved.matches_id(connection_id) && !saved.disabled)
            .ok_or("Saved connection unavailable")?;
        if !matches!(
            saved.kind,
            ConnectionKind::Postgres | ConnectionKind::Sqlite
        ) {
            return Err("This database does not expose SQL schemas".into());
        }
        let epoch = self
            .sessions
            .generation(connection_id)
            .ok_or("Connection unavailable")?;
        let session = self
            .driver_runtime
            .sessions
            .get(connection_id)
            .cloned()
            .ok_or("Connection unavailable")?;
        let token = self
            .filter_enumeration
            .begin_schema(connection_id, database, epoch);
        let supports_schemas = self.sql_supports_schemas(connection_id);
        let connection = connection_id.to_string();
        let database = database.to_string();
        let request_database = database.clone();
        cx.notify();
        let work = cx.background_executor().spawn(async move {
            let load = crate::navigator::LoadRequest::Schemas {
                connection: String::new(),
                database: request_database,
            };
            let names = match crate::navigator::sql_driver::catalog_request(&load, supports_schemas, None) {
                Some(request) => smol::unblock(move || {
                    crate::navigator::sql_driver::collect_pages(request, false, |request| {
                        super::navigator_ui::list_driver_nav_nodes(&session, request)
                    })
                })
                .await
                .map(|nodes| {
                    crate::navigator::sql_driver::catalog_children(&load, &nodes)
                        .into_iter()
                        .map(|schema| schema.label)
                        .collect::<Vec<_>>()
                })
                .map_err(|error| error.to_string())?,
                // No schema level: the one schema is the database.
                None => match load {
                    crate::navigator::LoadRequest::Schemas { database, .. } => vec![database],
                    _ => Vec::new(),
                },
            };
            if names.len() > MAX_FILTER_NAMES {
                return Err("Schema list exceeds the supported limit".into());
            }
            Ok::<_, String>(names)
        });
        cx.spawn(async move |this, cx| {
            let result = work
                .await
                .map_err(|error| crate::sessions::redact_secrets(&error));
            let _ = this.update(cx, |this, cx| {
                if this.sessions.generation(&connection) == Some(epoch)
                    && this.sessions.is_connected(&connection)
                {
                    this.filter_enumeration.finish_schema(
                        &connection,
                        &database,
                        token,
                        epoch,
                        result,
                    );
                    this.sync_visible_filter_schema_dialog(&connection, &database, cx);
                } else {
                    this.filter_enumeration
                        .cancel_schema_if(&connection, &database, token);
                }
                cx.notify();
            });
        })
        .detach();
        Ok(())
    }

    pub(super) fn visible_filter_names(&self, connection_id: &str) -> Option<&[String]> {
        self.filter_enumeration.names(connection_id)
    }

    pub(super) fn visible_filter_loading(&self, connection_id: &str) -> bool {
        self.filter_enumeration.loading(connection_id)
    }

    pub(super) fn visible_filter_error(&self, connection_id: &str) -> Option<&str> {
        self.filter_enumeration.error(connection_id)
    }

    pub(super) fn request_visible_filter_names(
        &mut self,
        connection_id: &str,
        cx: &mut gpui::Context<Self>,
    ) -> Result<(), String> {
        self.request_database_names(connection_id, cx)
    }

    fn request_database_names(
        &mut self,
        connection_id: &str,
        cx: &mut gpui::Context<Self>,
    ) -> Result<(), String> {
        if !self
            .saved_connections
            .iter()
            .any(|saved| saved.matches_id(connection_id) && !saved.disabled)
        {
            return Err("Saved connection is unavailable".into());
        }
        let epoch = self
            .sessions
            .generation(connection_id)
            .ok_or("Connection is unavailable")?;
        let session = self
            .driver_runtime
            .sessions
            .get(connection_id)
            .cloned()
            .ok_or("Driver session is unavailable")?;
        let connection = connection_id.to_string();
        let (token, cancelled) = self.filter_enumeration.begin(connection_id, epoch);
        cx.notify();
        let work = cx.background_executor().spawn(async move {
            smol::unblock(move || {
                enumerate_provider_databases(&cancelled, |request| {
                    let json = serde_json::to_string(&request)
                        .map_err(|_| "Invalid navigator request")?;
                    session
                        .lock()
                        .map_err(|_| "Driver session is unavailable".to_string())?
                        .list_nav_nodes(&json)
                        .map_err(|error| error.to_string())
                })
            })
            .await
        });
        cx.spawn(async move |this, cx| {
            let result = work
                .await
                .map_err(|error| crate::sessions::redact_secrets(&error));
            let _ = this.update(cx, |this, cx| {
                let current_epoch = this.sessions.generation(&connection);
                if current_epoch == Some(epoch) && this.sessions.is_connected(&connection) {
                    this.filter_enumeration.finish(&connection, token, epoch, result);
                } else {
                    this.filter_enumeration.cancel_if(&connection, token);
                }
                // UI observation only: mirror the outcome into an open filter
                // dialog's draft so async completion lands without reopen.
                // Product enumeration state above is untouched.
                this.sync_visible_filter_dialog(&connection, cx);
                cx.notify();
            });
        })
        .detach();
        Ok(())
    }

    pub(super) fn cancel_visible_filter_names(
        &mut self,
        connection_id: &str,
        cx: &mut gpui::Context<Self>,
    ) {
        self.filter_enumeration.cancel(connection_id);
        // UI observation only; see the enumeration continuation above.
        self.sync_visible_filter_dialog(connection_id, cx);
        cx.notify();
    }

    pub(super) fn apply_visible_filter(
        &mut self,
        connection_id: &str,
        filter: crate::sidebar_preferences::VisibleObjectFilter,
        cx: &mut gpui::Context<Self>,
    ) -> Result<(), String> {
        if self.filter_enumeration.completed_epoch(connection_id)
            != self.sessions.generation(connection_id)
        {
            return Err("Database list is stale; reopen the filter".into());
        }
        let connection = connection_id.to_string();
        self.change_sidebar_preferences(|prefs| prefs.set_visible_filter(&connection, filter), cx)
    }

    #[cfg(test)]
    pub(super) fn show_all_visible_objects(
        &mut self,
        connection_id: &str,
        cx: &mut gpui::Context<Self>,
    ) -> Result<(), String> {
        let connection = connection_id.to_string();
        self.change_sidebar_preferences(
            |prefs| {
                prefs.show_all_objects(&connection);
                Ok(())
            },
            cx,
        )
    }

    pub(super) fn can_create_database(&self, connection_id: &str) -> bool {
        let Some(saved) = self
            .saved_connections
            .iter()
            .find(|saved| saved.matches_id(connection_id) && !saved.disabled)
        else {
            return false;
        };
        if !self.sessions.is_connected(connection_id) {
            return false;
        }
        // Cached capabilities: a session busy with navigator I/O must not
        // hide "Create Database".
        self.driver_capabilities(connection_id, saved.kind.driver_id())
            .is_some_and(|capabilities| capabilities.has(Capability::DatabaseCreate))
    }

    pub(super) fn copy_connection_info(
        &mut self,
        connection_id: &str,
        item: CopyInfo,
        cx: &mut gpui::Context<Self>,
    ) {
        if self.scenario_ui.service.is_some() {
            return;
        }
        let result = self
            .saved_connections
            .iter()
            .find(|saved| saved.matches_id(connection_id))
            .ok_or_else(|| "Saved connection not found".to_string())
            .and_then(|saved| copy_saved_connection_info(saved, item));
        match result {
            Ok(text) => cx.write_to_clipboard(gpui::ClipboardItem::new_string(text)),
            Err(error) => {
                self.sidebar.notice = Some(format!("Could not copy connection info: {error}"));
                self.report_error(Area::Connect, Some(connection_id), format!("Could not copy connection info: {error}"));
                cx.notify();
            }
        }
    }

    pub(super) fn saved_query_history(
        &self,
        connection_id: &str,
    ) -> (
        bool,
        Option<&[crate::saved_sql::SavedSqlEntry]>,
        Option<&str>,
    ) {
        (
            self.saved_sql_ui.loading(connection_id),
            self.saved_sql_ui.entries(connection_id),
            self.saved_sql_ui.error(connection_id),
        )
    }

    pub(super) fn request_saved_query_history(
        &mut self,
        connection_id: &str,
        cx: &mut gpui::Context<Self>,
    ) -> Result<(), String> {
        let Some(kind) = self
            .saved_connections
            .iter()
            .find(|saved| saved.matches_id(connection_id) && !saved.disabled)
            .map(|saved| saved.kind)
        else {
            return Err("Saved connection is unavailable".into());
        };
        let connection = connection_id.to_string();
        let epoch = self.sessions.generation(connection_id).unwrap_or(0);
        let (base, include_registered) = self.saved_sql_ui.source()?;
        let token = self.saved_sql_ui.begin(connection_id, epoch);
        cx.notify();
        let work = cx.background_executor().spawn(async move {
            smol::unblock(move || {
                let roots = crate::saved_sql::project_roots_at(&base, include_registered)?;
                if kind == ConnectionKind::Mongodb {
                    crate::saved_sql::recent_scripts(&roots, &connection)
                } else {
                    crate::saved_sql::recent_sql(&roots, &connection)
                }
            })
            .await
        });
        let connection = connection_id.to_string();
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |this, cx| {
                if this.sessions.generation(&connection).unwrap_or(0) == epoch {
                    if this.saved_sql_ui.finish(&connection, token, epoch, result) {
                        this.sync_saved_history_menu(&connection, epoch, cx);
                    }
                } else {
                    this.saved_sql_ui.cancel_if(&connection, token);
                }
                cx.notify();
            });
        })
        .detach();
        Ok(())
    }

    pub(super) fn save_active_sql_as(
        &mut self,
        name: &str,
        cx: &mut gpui::Context<Self>,
    ) -> Result<(), String> {
        let tab = self.workspace.active_tab().ok_or("No active query")?;
        let saved_kind = self
            .saved_connections
            .iter()
            .find(|saved| saved.matches_id(&tab.connection_id) && !saved.disabled)
            .map(|saved| saved.kind)
            .ok_or("Saved connection is unavailable")?;
        if tab.kind != crate::workspace::TabKind::Query
            || tab.query_mode.is_mongo_find()
        {
            return Err("Only editable queries can be saved".into());
        }
        let is_mongo_script = tab.query_mode.is_mongo_script()
            && saved_kind == ConnectionKind::Mongodb;
        if (tab.query_mode.is_mongo_script())
            != (saved_kind == ConnectionKind::Mongodb)
            || (!is_mongo_script
                && !matches!(
                    saved_kind,
                    ConnectionKind::Postgres | ConnectionKind::Sqlite
                ))
        {
            return Err("Query language does not match the saved connection".into());
        }
        let connection = tab.connection_id.clone();
        let database = tab.database.clone();
        let sql = tab.sql.clone();
        let (saved_tab, saved_text) = (tab.id, sql.clone());
        let name = name.to_string();
        let now = super::current_time();
        let epoch = self.sessions.generation(&connection).unwrap_or(0);
        let (base, include_registered) = self.saved_sql_ui.source()?;
        let work = cx.background_executor().spawn(async move {
            smol::unblock(move || {
                let root = crate::saved_sql::project_roots_at(&base, include_registered)?
                    .into_iter()
                    .next()
                    .ok_or("General scripts project unavailable")?;
                if is_mongo_script {
                    crate::saved_sql::save_script(&root, &connection, &database, &name, &sql, now)
                } else {
                    crate::saved_sql::save_sql(&root, &connection, &database, &name, &sql, now)
                }
            })
            .await
        });
        let connection = tab.connection_id.clone();
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |this, cx| {
                if result.is_ok() {
                    // The script on disk equals this text (unsaved-close check).
                    this.settings_ui.saved_scripts.insert(saved_tab, saved_text);
                }
                match result {
                    Ok(_) if this.sessions.generation(&connection).unwrap_or(0) == epoch => {
                        let _ = this.request_saved_query_history(&connection, cx);
                    }
                    Ok(_) => {
                        this.report_warning(Area::Query, Some(&connection), "SQL script was saved, but the connection changed; reopen its history");
                        this.sidebar.notice = Some(
                            "SQL script was saved, but the connection changed; reopen its history"
                                .into(),
                        )
                    }
                    Err(error) => {
                        this.report_error(Area::Query, Some(&connection), format!("Could not save SQL script: {error}"));
                        this.sidebar.notice = Some(format!("Could not save SQL script: {error}"))
                    }
                }
                cx.notify();
            });
        })
        .detach();
        Ok(())
    }

    pub(super) fn open_saved_sql_entry(
        &mut self,
        entry: &crate::saved_sql::SavedSqlEntry,
        connection_id: &str,
        cx: &mut gpui::Context<Self>,
    ) -> Result<(), String> {
        if !self.saved_connections.iter().any(|saved| {
            saved.matches_id(connection_id)
                && !saved.disabled
                && match saved.kind {
                    ConnectionKind::Mongodb => entry.dialect == QueryMode::MongoScript.as_str(),
                    ConnectionKind::Postgres | ConnectionKind::Sqlite => entry.dialect == "sql",
                    _ => false,
                }
        }) {
            return Err("Saved connection is unavailable".into());
        }
        if !uuid::Uuid::parse_str(connection_id)
            .is_ok_and(|id| id.to_string() == entry.connection_id)
        {
            return Err("Saved SQL belongs to a different connection".into());
        }
        let connection = connection_id.to_string();
        let epoch = self.sessions.generation(connection_id).unwrap_or(0);
        let dialect = entry.dialect.clone();
        let entry = entry.clone();
        let work = cx.background_executor().spawn(async move {
            smol::unblock(move || {
                let sql = crate::saved_sql::open_sql(&entry)?;
                Ok::<_, String>((entry, sql))
            })
            .await
        });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |this, cx| {
                if this.sessions.generation(&connection).unwrap_or(0) != epoch
                    || !this.saved_connections.iter().any(|saved| {
                        saved.matches_id(&connection)
                            && !saved.disabled
                            && match saved.kind {
                                ConnectionKind::Mongodb => dialect == QueryMode::MongoScript.as_str(),
                                ConnectionKind::Postgres | ConnectionKind::Sqlite => {
                                    dialect == "sql"
                                }
                                _ => false,
                            }
                    })
                {
                    this.report_warning(Area::Query, Some(&connection), "Saved SQL was not opened because the connection changed");
                    this.sidebar.notice =
                        Some("Saved SQL was not opened because the connection changed".into());
                } else {
                    match result.and_then(|(entry, sql)| {
                        open_saved_query_with_contents(
                            &mut this.workspace,
                            &entry,
                            &connection,
                            sql,
                        )
                    }) {
                        Ok(_) => {}
                        Err(error) => {
                            this.report_error(Area::Query, Some(&connection), format!("Could not open saved SQL: {error}"));
                            this.sidebar.notice = Some(format!("Could not open saved SQL: {error}"))
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connections::ConnectionKind;
    use uuid::Uuid;

    fn saved(kind: ConnectionKind, uri: &str, save_password: bool) -> SavedConnection {
        SavedConnection {
            id: Uuid::nil(),
            name: "Fixture".into(),
            uri: uri.into(),
            kind,
            save_password,
            is_pinned: false,
            disabled: false,
            ssh_tunnel: false,
        }
    }

    #[test]
    fn copy_safe_url_never_resolves_secrets_and_strips_query_and_proxy_credentials() {
        let conn = saved(ConnectionKind::Mongodb,
            "mongodb://user:old@a:27017,b:27018/db?authSource=admin&proxyPassword=hidden&token=secret#private", true);
        let value = copy_info(&conn, CopyInfo::ConnectionUrl, |_| {
            panic!("must not read keychain")
        })
        .unwrap();
        assert_eq!(value, "mongodb://user@a:27017,b:27018/db?authSource=admin");
    }

    #[test]
    fn copy_with_password_is_explicit_encoded_and_fails_closed() {
        let conn = saved(
            ConnectionKind::Mongodb,
            "mongodb+srv://user@cluster.test/db?tls=true",
            true,
        );
        assert_eq!(
            copy_info(&conn, CopyInfo::ConnectionUrlWithPassword, |_| Some(
                "p@ss:word".into()
            ))
            .unwrap(),
            "mongodb+srv://user:p%40ss%3Aword@cluster.test/db?tls=true"
        );
        assert!(copy_info(&conn, CopyInfo::ConnectionUrlWithPassword, |_| None).is_err());
        assert!(
            copy_info(&conn, CopyInfo::ConnectionUrlWithPassword, |_| Some(
                String::new()
            ))
            .is_err()
        );
        assert!(copy_info(
            &saved(ConnectionKind::Postgres, "postgres://host/db", false),
            CopyInfo::ConnectionUrlWithPassword,
            |_| panic!("not saved")
        )
        .is_err());
        let options = saved(
            ConnectionKind::Postgres,
            "postgres://user@host/db?sslmode=verify-full&customOption=required",
            true,
        );
        assert_eq!(
            copy_info(&options, CopyInfo::ConnectionUrlWithPassword, |_| Some(
                "fixture".into()
            ))
            .unwrap(),
            "postgres://user:fixture@host/db?sslmode=verify-full&customOption=required"
        );
    }

    #[test]
    fn host_port_handles_seed_lists_srv_ipv6_no_port_and_sqlite() {
        for (kind, uri, expected) in [
            (
                ConnectionKind::Mongodb,
                "mongodb://user@a:27017,b:27018/db",
                "a:27017,b:27018",
            ),
            (
                ConnectionKind::Mongodb,
                "mongodb+srv://cluster.test/db",
                "cluster.test",
            ),
            (
                ConnectionKind::Postgres,
                "postgresql://[::1]:5432/db",
                "[::1]:5432",
            ),
            (ConnectionKind::Postgres, "postgresql://host/db", "host"),
        ] {
            assert_eq!(
                copy_info(
                    &saved(kind, uri, false),
                    CopyInfo::HostPort,
                    |_| unreachable!()
                )
                .unwrap(),
                expected
            );
        }
        assert!(copy_info(
            &saved(ConnectionKind::Sqlite, "sqlite:///tmp/a#b.db", false),
            CopyInfo::HostPort,
            |_| unreachable!()
        )
        .is_err());
        assert!(copy_info(
            &saved(
                ConnectionKind::Mongodb,
                "mongodb://a:27017,broken:abc/db",
                false
            ),
            CopyInfo::HostPort,
            |_| unreachable!()
        )
        .is_err());
        assert!(copy_info(
            &saved(ConnectionKind::Mongodb, "mongodb+srv://a,b/db", false),
            CopyInfo::ConnectionUrl,
            |_| unreachable!()
        )
        .is_err());
    }

    #[test]
    fn saved_query_menu_flow_saves_active_sql_and_opens_scoped_tab_without_running_it() {
        let root = std::env::temp_dir().join(format!("tablex-menu-sql-{}", Uuid::new_v4()));
        let id = "11111111-1111-1111-1111-111111111111";
        let mut workspace = crate::workspace::WorkspaceState::new();
        workspace.open_query_tab_with_metadata(
            "Query",
            crate::workspace::TabMetadata {
                connection_id: id.into(),
                database: "app".into(),
                ..Default::default()
            },
        );
        workspace.active_tab_mut().unwrap().sql = "SELECT * FROM orders".into();
        let entry = save_active_query(&workspace, &root, "Orders", 42.0).unwrap();
        let recent = crate::saved_sql::recent_sql(std::slice::from_ref(&root), id).unwrap();
        assert_eq!(recent, std::slice::from_ref(&entry));
        let tab_id = open_saved_query(&mut workspace, &entry, id).unwrap();
        let tab = workspace.active_tab().unwrap();
        assert_eq!(tab.id, tab_id);
        assert_eq!(tab.sql, "SELECT * FROM orders");
        assert_eq!(tab.connection_id, id);
        assert_eq!(tab.database, "app");
        assert!(matches!(
            workspace.runtime(tab_id).unwrap().query.status(),
            crate::query::QueryStatus::Idle
        ));
        std::fs::remove_file(&entry.path).unwrap();
        assert!(open_saved_query(&mut workspace, &entry, id).is_err());
        assert_eq!(workspace.active_tab().unwrap().id, tab_id);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn saved_mongo_script_opens_typed_editor_without_executing_or_becoming_sql() {
        let root = std::env::temp_dir().join(format!("tablex-script-menu-{}", Uuid::new_v4()));
        let id = "11111111-1111-1111-1111-111111111111";
        let entry = crate::saved_sql::save_script(
            &root,
            id,
            "warehouse",
            "Orders",
            "db.orders.find({})",
            4.0,
        )
        .unwrap();
        let mut workspace = crate::workspace::WorkspaceState::new();
        let tab_id = open_saved_query(&mut workspace, &entry, id).unwrap();
        let tab = workspace.active_tab().unwrap();
        assert_eq!(tab.query_mode, Some(QueryMode::MongoScript));
        assert_eq!(tab.database, "warehouse");
        assert_eq!(tab.sql, "db.orders.find({})");
        assert!(matches!(
            workspace.runtime(tab_id).unwrap().query.status(),
            crate::query::QueryStatus::Idle
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn filter_enumeration_collects_all_pages_rejects_cursor_cycles_and_keeps_epoch() {
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let names = enumerate_provider_databases(&cancel, |request| {
            Ok(engine_driver_api::NavigatorPage {
                nodes: vec![engine_driver_api::NavigatorNode {
                    id: "db".into(),
                    kind: "database".into(),
                    label: "db".into(),
                    subtitle: None,
                    icon: None,
                    has_children: true,
                    payload: serde_json::json!({"database":"db"}),
                    actions: vec![],
                }],
                next_cursor: if request.cursor.is_none() {
                    Some("page2".into())
                } else {
                    None
                },
            })
        })
        .unwrap();
        assert_eq!(names, vec!["db"]);
        assert!(
            enumerate_provider_databases(&cancel, |_| Ok(engine_driver_api::NavigatorPage {
                nodes: vec![],
                next_cursor: Some("loop".into()),
            }))
            .is_err()
        );
        let mut state = FilterEnumerationState::default();
        let old = state.begin("conn", 1);
        let new = state.begin("conn", 2);
        state.cancel_if("conn", old.0);
        assert!(state.loading("conn"));
        assert!(!state.finish("conn", old.0, 1, Ok(names.clone())));
        assert!(!state.finish("conn", new.0, 1, Ok(names.clone())));
        assert!(state.finish("conn", new.0, 2, Ok(names)));
        assert_eq!(state.names("conn").unwrap(), ["db"]);
        let old_schema = state.begin_schema("conn", "db", 2);
        let current_schema = state.begin_schema("conn", "db", 3);
        assert!(!state.finish_schema("conn", "db", old_schema, 2, Ok(vec!["stale".into()])));
        assert!(!state.finish_schema("conn", "db", current_schema, 2, Ok(vec!["stale".into()])));
        assert!(state.finish_schema("conn", "db", current_schema, 3, Ok(vec!["public".into()])));
        assert_eq!(state.schema_names("conn", "db").unwrap(), ["public"]);
    }

    #[test]
    fn saved_sql_history_cache_rejects_stale_async_results() {
        let mut state = SavedSqlUiState::default();
        let old = state.begin("conn", 1);
        let current = state.begin("conn", 2);
        state.cancel_if("conn", old);
        assert!(state.loading("conn"));
        assert!(!state.finish("conn", old, 1, Ok(vec![])));
        assert!(!state.finish("conn", current, 1, Ok(vec![])));
        assert!(state.finish("conn", current, 2, Ok(vec![])));
        assert_eq!(state.entries("conn").unwrap().len(), 0);
        assert!(!state.loading("conn"));
    }

    #[test]
    fn filter_draft_selects_all_filtered_clear_show_all_and_system() {
        let mut draft = VisibleFilterDraft::new(
            vec!["admin".into(), "analytics".into(), "archive".into()],
            crate::sidebar_preferences::VisibleObjectFilter::default(),
        );
        draft.search = "analy".into();
        draft.select_filtered();
        assert_eq!(draft.filter.databases, Some(vec!["analytics".into()]));
        draft.select_all();
        assert_eq!(draft.filter.databases.as_ref().unwrap().len(), 3);
        draft.clear();
        assert_eq!(draft.filter.databases, Some(vec![]));
        draft.show_all();
        assert_eq!(draft.filter.databases, None);
        draft.filter.show_system = false;
        assert!(!draft.displayed().contains(&"admin"));
    }
}
