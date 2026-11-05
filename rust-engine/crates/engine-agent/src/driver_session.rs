//! The one real [`Session`]: an adapter over `engine_driver_host`'s
//! [`SharedDriverSession`]. A front end only has to open the driver session
//! (credentials, tunnel, URI, driver loading) and wrap it:
//!
//! ```ignore
//! let driver_session = handle.connect(&request_json)?;
//! let session = DriverSessionAdapter::new(handle, driver_session).with_secrets(vec![password]);
//! ```
//!
//! Statements run through a lock-free [`QueryRunner`](engine_driver_host::QueryRunner)
//! (a slow query never queues the navigator behind it); the session lock is
//! taken only to read the finished page, whose cells are rendered as text in
//! small chunks so the byte cap stops the read early.

use crate::cancel::CancelToken;
use crate::error::AgentError;
use crate::limits::RowCollector;
use crate::ports::{
    ConnectionKind, Language, Node, NodeLevel, NodePage, NodeQuery, RunRequest, Session,
    SessionInfo, TableRef,
};
use crate::result::{QueryResult, ResultColumn};
use crate::secret::SecretText;
use crate::table::TableDescription;
use engine_driver_api::sql::{SqlColumnNode, SqlObjectRef, TableColumn, TableProperties};
use engine_driver_api::{
    CancelRequest, Capability, NavigatorRequest, QueryRequest, SqlDialectInfo,
};
use engine_driver_host::{DriverHandle, DriverSession, LoaderError, SharedDriverSession};
use engine_driver_kit::sql::SqlLexOptions;
use serde_json::{json, Value};
use std::sync::Arc;

/// Rows rendered per `render_cell_batch` call.
const READ_CHUNK_ROWS: u64 = 64;
const TABLE_KINDS: [&str; 4] = ["table", "view", "materialized_view", "collection"];

pub struct DriverSessionAdapter {
    handle: Arc<DriverHandle>,
    session: Arc<SharedDriverSession>,
    connection_id: u64,
    kind: ConnectionKind,
    lex: Option<SqlLexOptions>,
    secrets: Vec<SecretText>,
}

impl DriverSessionAdapter {
    /// Wraps a freshly connected driver session. Fails with
    /// `agent.unsupported` when the driver is not one of the engines the
    /// agent tools know.
    pub fn new(handle: Arc<DriverHandle>, session: DriverSession) -> Result<Self, AgentError> {
        let capabilities = handle.capabilities();
        let kind = ConnectionKind::from_driver_id(&capabilities.driver).ok_or_else(|| {
            AgentError::Unsupported(format!(
                "driver `{}` is not supported by the agent tools",
                capabilities.driver
            ))
        })?;
        let lex = session_lex(
            kind,
            session
                .connection_dialect()
                .or(capabilities.sql_dialect.as_ref()),
            session.server_version(),
        );
        let connection_id = session.connection_id();
        Ok(Self {
            handle,
            session: Arc::new(SharedDriverSession::new(session)),
            connection_id,
            kind,
            lex,
            secrets: Vec::new(),
        })
    }

    /// Exact secret values to scrub from errors and the audit log.
    #[must_use]
    pub fn with_secrets(mut self, secrets: Vec<SecretText>) -> Self {
        self.secrets = secrets;
        self
    }

    /// The shared driver session, for a front end that needs to release or
    /// inspect it.
    ///
    /// **The agent must have its OWN driver session, never the user's
    /// interactive one.** Read-only execution is applied to the connection while
    /// a statement runs (SQLite `PRAGMA query_only` is connection-wide, MySQL and
    /// PostgreSQL wrap the statement in a read-only transaction), and an open
    /// transaction of the user's decides the mode (MySQL refuses with
    /// `query.read_only_unavailable`; PostgreSQL falls back to a savepoint). Open
    /// a separate session in [`SessionProvider::open`](crate::SessionProvider::open)
    /// and, where the database has one, connect it with a **read-only role** for
    /// agent access `read` and below: a read is only as strong as the database
    /// user it runs as (foreign-data-wrapper tables, user-defined functions and
    /// extensions are outside the classifier and the denylist).
    pub fn shared(&self) -> &Arc<SharedDriverSession> {
        &self.session
    }

    fn navigate(
        &self,
        request: &NavigatorRequest,
    ) -> Result<engine_driver_api::NavigatorPage, AgentError> {
        let json =
            serde_json::to_string(request).map_err(|e| AgentError::Internal(e.to_string()))?;
        let mut session = self
            .session
            .lock()
            .map_err(|_| AgentError::Internal("driver session lock poisoned".into()))?;
        session.list_nav_nodes(&json).map_err(map_error)
    }

    fn rich_describe(&self) -> bool {
        self.handle
            .capabilities()
            .has(Capability::MetadataTableProperties)
    }

    fn describe_from_columns(&self, table: &TableRef) -> Result<TableDescription, AgentError> {
        let object = object_ref(table);
        let payload =
            serde_json::to_value(&object).map_err(|e| AgentError::Internal(e.to_string()))?;
        let page = self.navigate(&NavigatorRequest {
            parent: None,
            database: table.database.clone(),
            payload: Some(payload),
            cursor: None,
            limit: 5000,
        })?;
        let columns: Vec<TableColumn> = page
            .nodes
            .iter()
            .filter(|node| node.kind == "column")
            .filter_map(|node| serde_json::from_value::<SqlColumnNode>(node.payload.clone()).ok())
            .map(|node| node.details)
            .collect();
        if columns.is_empty() {
            return Err(AgentError::driver(
                "describe.not_found",
                format!("no table or view named `{}`", table.name),
                false,
            ));
        }
        Ok(TableDescription::from_columns(
            table.name.clone(),
            table.database.clone(),
            table.schema.clone(),
            &columns,
        ))
    }

    /// Reads the rows of an open page into a collector, in chunks.
    fn read_rows(
        &self,
        page: &engine_driver_api::QueryPage,
        collector: &mut RowCollector,
    ) -> Result<(), AgentError> {
        let columns = collector.column_count() as u64;
        let mut session = self
            .session
            .lock()
            .map_err(|_| AgentError::Internal("driver session lock poisoned".into()))?;
        let mut start = 0;
        while start < page.row_count {
            let count = READ_CHUNK_ROWS.min(page.row_count - start);
            let batch = session
                .render_cell_batch(page.page_id, start, count, 0, columns)
                .map_err(map_error)?;
            let mut cells = (0..batch.cell_count()).map(|index| batch.cell(index));
            for _ in 0..count {
                let mut row = Vec::with_capacity(columns as usize);
                for _ in 0..columns {
                    let cell = cells.next().flatten().ok_or_else(|| {
                        AgentError::Internal("the driver returned a short cell batch".into())
                    })?;
                    row.push(cell.text);
                }
                if collector.push_row(row).is_break() {
                    return Ok(());
                }
            }
            start += count;
        }
        Ok(())
    }
}

fn object_ref(table: &TableRef) -> SqlObjectRef {
    SqlObjectRef {
        database: table.database.clone(),
        schema: table.schema.clone(),
        object: table.name.clone(),
    }
}

/// Maps a driver error to the tool layer's error, keeping its code.
pub fn map_error(error: LoaderError) -> AgentError {
    match error {
        LoaderError::DriverRejected {
            code,
            message,
            retryable,
        } => {
            if code == "query.read_only_violation" {
                AgentError::WriteRefused(format!(
                    "the statement tried to write, but statements the policy treats as reads run read-only: {message}"
                ))
            } else if code.ends_with(".cancelled") {
                AgentError::Cancelled
            } else if code.ends_with(".not_connected") {
                AgentError::NotConnected(format!(
                    "the connection was lost ({message}); call open_session again"
                ))
            } else if code == engine_driver_api::sql::codes::QUERY_CONFIRMATION_REQUIRED {
                AgentError::DangerousRefused(format!("the driver asks for confirmation: {message}"))
            } else {
                AgentError::Driver {
                    code,
                    message,
                    retryable,
                }
            }
        }
        LoaderError::UnsupportedFeature { feature } => {
            AgentError::Unsupported(format!("the driver does not support `{feature}`"))
        }
        other => AgentError::driver("driver.error", other.to_string(), false),
    }
}

/// The lexer options this connection's SQL is read with: the engine's preset moved
/// by the dialect the session reported (its `sql_mode` for MySQL/MariaDB, else the
/// driver's static one), and read as the server version reads versioned comments.
fn session_lex(
    kind: ConnectionKind,
    dialect: Option<&SqlDialectInfo>,
    server_version: Option<u32>,
) -> Option<SqlLexOptions> {
    let mut lex = kind.lex_options()?;
    if let Some(dialect) = dialect {
        lex = lex.with_session_dialect(dialect);
    }
    if let Some(version) = server_version {
        lex = lex.with_server_version(version);
    }
    Some(lex)
}

impl Session for DriverSessionAdapter {
    fn info(&self) -> SessionInfo {
        let dialect = self.handle.capabilities().sql_dialect.as_ref();
        SessionInfo {
            kind: self.kind,
            language: self.kind.language(),
            lex: self.lex,
            supports_schemas: dialect.is_some_and(|dialect| dialect.supports_schemas),
            rich_describe: self.rich_describe(),
        }
    }

    fn list_nodes(&self, level: NodeLevel, query: &NodeQuery) -> Result<NodePage, AgentError> {
        let mut payload = serde_json::Map::new();
        if let Some(database) = &query.database {
            payload.insert("database".into(), json!(database));
        }
        if level == NodeLevel::Tables {
            if let Some(schema) = &query.schema {
                payload.insert("schema".into(), json!(schema));
            }
            if let Some(filter) = &query.filter {
                payload.insert("filter".into(), json!(filter));
            }
        }
        let page = self.navigate(&NavigatorRequest {
            parent: None,
            database: query.database.clone(),
            payload: (!payload.is_empty()).then_some(Value::Object(payload)),
            cursor: query.cursor.clone(),
            limit: query.limit as u64,
        })?;
        let wanted = |kind: &str| match level {
            NodeLevel::Databases => kind == "database",
            NodeLevel::Schemas => kind == "schema",
            NodeLevel::Tables => TABLE_KINDS.contains(&kind),
        };
        let nodes = page
            .nodes
            .into_iter()
            .filter(|node| wanted(&node.kind))
            .map(|node| {
                // The payload names the object exactly (the label may carry
                // counts, e.g. Redis `db0 (3 keys)`).
                let key = match level {
                    NodeLevel::Databases => "database",
                    NodeLevel::Schemas => "schema",
                    NodeLevel::Tables => "object",
                };
                let name = node
                    .payload
                    .get(key)
                    .and_then(Value::as_str)
                    .map_or(node.label, str::to_owned);
                Node {
                    name,
                    kind: node.kind,
                }
            })
            .collect();
        Ok(NodePage {
            nodes,
            next_cursor: page.next_cursor,
        })
    }

    fn describe_table(&self, table: &TableRef) -> Result<TableDescription, AgentError> {
        if !self.rich_describe() {
            return self.describe_from_columns(table);
        }
        let request = serde_json::to_value(object_ref(table))
            .map_err(|e| AgentError::Internal(e.to_string()))?;
        let response = self.session.describe_object(&request).map_err(map_error)?;
        let properties: TableProperties = serde_json::from_value(response)
            .map_err(|error| AgentError::driver("describe.malformed", error.to_string(), false))?;
        Ok(TableDescription::from_properties(
            table.name.clone(),
            table.database.clone(),
            table.schema.clone(),
            properties,
            usize::MAX,
        ))
    }

    fn run(
        &self,
        request: &RunRequest<'_>,
        cancel: &CancelToken,
    ) -> Result<QueryResult, AgentError> {
        let capabilities = self.handle.capabilities();
        // Read-only execution is requested only where the driver advertises it.
        let read_only = request.read_only
            && request.language == Language::Sql
            && capabilities.has(Capability::QueryReadOnly);
        let (kind, mut payload) = match request.language {
            Language::Sql if capabilities.has(Capability::QuerySql) => {
                ("sql", json!({"sql": request.text}))
            }
            Language::Redis if capabilities.has(Capability::QueryRedisCommand) => {
                ("redis.command", json!({"command": request.text}))
            }
            _ => {
                return Err(AgentError::Unsupported(
                    "the driver cannot run this kind of statement".into(),
                ))
            }
        };
        if request.confirmed {
            payload["confirmed"] = json!(true);
        }
        if read_only {
            payload["read_only"] = json!(true);
        }
        if cancel.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        // A lock-free runner: the operation id is registered before the
        // request carries it, and cancel can reach it from any thread.
        let runner = self.session.query_runner().map_err(map_error)?;
        let operation_id = runner.operation_id();
        let _hook = {
            let handle = Arc::clone(&self.handle);
            let connection_id = self.connection_id;
            cancel.on_cancel(move || {
                let _ = handle.cancel_operation(connection_id, &CancelRequest { operation_id });
            })
        };
        let query = serde_json::to_string(&QueryRequest {
            kind: kind.into(),
            operation_id,
            database: request
                .database
                .map(str::to_owned)
                .filter(|database| !database.is_empty()),
            object: None,
            offset: 0,
            limit: request.row_limit as u64,
            payload,
        })
        .map_err(|e| AgentError::Internal(e.to_string()))?;
        let page = runner.open_query(&query).map_err(map_error)?;
        // Register the page with the session (so a disconnect releases it) and
        // always release it once read.
        {
            let mut session = self.session.lock().map_err(|_| {
                runner.release_page(page.page_id);
                AgentError::Internal("driver session lock poisoned".into())
            })?;
            session
                .adopt_page(&runner, page.page_id)
                .map_err(map_error)?;
        }
        let mut collector =
            RowCollector::new(page_columns(&page), request.row_limit, request.limits);
        let read = self.read_rows(&page, &mut collector);
        // Release regardless of the read outcome.
        if let Ok(mut session) = self.session.lock() {
            let _ = session.release_page(page.page_id);
        }
        read?;
        let affected = page
            .columns
            .is_empty()
            .then_some(page.total_count)
            .flatten();
        let mut result = collector.finish(page.flags.truncated, affected);
        result.read_only_enforced = read_only;
        Ok(result)
    }

    fn disconnect(&self) {
        if let Ok(mut session) = self.session.lock() {
            let _ = session.disconnect();
        }
    }

    fn secrets(&self) -> Vec<SecretText> {
        self.secrets.clone()
    }
}

fn page_columns(page: &engine_driver_api::QueryPage) -> Vec<ResultColumn> {
    page.columns
        .iter()
        .map(|column| ResultColumn {
            name: column.name.clone(),
            data_type: column.data_type.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::functions::blocked_function;

    fn dialect(backslash: bool, double_quote: bool, bracket: bool) -> SqlDialectInfo {
        SqlDialectInfo {
            identifier_quote: "`".into(),
            string_literal_quote: "'".into(),
            supports_schemas: false,
            database_qualified_names: true,
            like_requires_text_cast: false,
            string_backslash_escapes: backslash,
            double_quote_identifiers: double_quote,
            bracket_identifiers: bracket,
        }
    }

    #[test]
    fn the_connection_dialect_decides_how_the_text_is_lexed() {
        let kind = ConnectionKind::MySql;
        let default = dialect(true, false, false);
        let ansi = dialect(true, true, false);
        let lex = |dialect: &SqlDialectInfo| session_lex(kind, Some(dialect), None).unwrap();
        assert_eq!(lex(&default), SqlLexOptions::mysql());
        assert!(lex(&ansi).double_quote_is_identifier);
        assert!(!lex(&dialect(false, false, false)).backslash_escapes);
        assert!(lex(&dialect(true, true, true)).bracket_identifiers);
        // No dialect at all: the preset; the version rides along.
        assert_eq!(session_lex(kind, None, None), kind.lex_options());
        assert_eq!(
            session_lex(kind, Some(&default), Some(80411)).map(|lex| lex.server_version),
            Some(Some(80411))
        );
        assert_eq!(session_lex(ConnectionKind::Redis, Some(&ansi), None), None);
    }

    #[test]
    fn ansi_quotes_unhide_a_function_the_default_lexer_reads_as_a_string() {
        // To an ANSI_QUOTES server `"a\"` is an identifier, so `load_file(...)` is code;
        // read with the default MySQL rules the whole tail is one string.
        let sql = r#"SELECT "a\" , load_file('/etc/passwd') -- ""#;
        let kind = ConnectionKind::MySql;
        let default = session_lex(kind, Some(&dialect(true, false, false)), None).unwrap();
        let ansi = session_lex(kind, Some(&dialect(true, true, false)), None).unwrap();
        assert_eq!(blocked_function(sql, kind, &default), None);
        assert_eq!(
            blocked_function(sql, kind, &ansi).as_deref(),
            Some("load_file")
        );
    }
}
