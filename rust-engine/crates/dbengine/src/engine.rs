use crate::catalog::{CatalogColumn, CatalogObject, CatalogObjectKind};
use crate::connection::ConnectionManager;
use crate::db::{postgres::PostgresConn, sqlite::SQLiteConn, QueryResult};
use crate::error::{DbEngineError, Result};
use crate::RUNTIME;
use arrow_array::RecordBatch;
use mongodb::options::ClientOptions;
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, RwLock};
use std::time::Duration;

const OPERATION_TIMEOUT_SECS: u64 = 30;

pub type ConnectionId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatabaseKind {
    Mongo,
    Postgres,
    Sqlite,
}

impl DatabaseKind {
    pub fn from_uri(uri: &str) -> Result<Self> {
        let lower = uri.trim().to_ascii_lowercase();
        if lower.starts_with("sqlite://") {
            Ok(Self::Sqlite)
        } else if lower.starts_with("postgres://") || lower.starts_with("postgresql://") {
            Ok(Self::Postgres)
        } else if lower.starts_with("mongodb://") || lower.starts_with("mongodb+srv://") {
            Ok(Self::Mongo)
        } else {
            // Do not echo the URI back: it can contain credentials.
            Err(DbEngineError::Connection(
                "Unsupported database URI scheme".into(),
            ))
        }
    }
}

#[derive(Debug, Clone)]
pub struct QueryRequest {
    pub connection_id: ConnectionId,
    pub database: String,
    pub sql: String,
    pub limit: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MutationValue {
    Null,
    Text(String),
    Number(String),
    Boolean(bool),
}

#[derive(Debug, Clone, PartialEq)]
pub struct UpdateCellRequest {
    pub connection_id: ConnectionId,
    pub database: String,
    pub schema: String,
    pub table: String,
    pub key_column: String,
    pub key_value: MutationValue,
    pub column: String,
    pub value: MutationValue,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeleteRowRequest {
    pub connection_id: ConnectionId,
    pub database: String,
    pub schema: String,
    pub table: String,
    pub key_column: String,
    pub key_value: MutationValue,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InsertRowRequest {
    pub connection_id: ConnectionId,
    pub database: String,
    pub schema: String,
    pub table: String,
    pub values: Vec<(String, MutationValue)>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SqlMutation {
    Update(UpdateCellRequest),
    Insert(InsertRowRequest),
    Delete(DeleteRowRequest),
}

impl SqlMutation {
    fn target(&self) -> (ConnectionId, &str, &str, &str) {
        match self {
            Self::Update(request) => (
                request.connection_id,
                &request.database,
                &request.schema,
                &request.table,
            ),
            Self::Insert(request) => (
                request.connection_id,
                &request.database,
                &request.schema,
                &request.table,
            ),
            Self::Delete(request) => (
                request.connection_id,
                &request.database,
                &request.schema,
                &request.table,
            ),
        }
    }

    fn sql(&self) -> std::result::Result<String, String> {
        match self {
            Self::Update(request) => build_update_cell_sql(request),
            Self::Insert(request) => build_insert_row_sql(request),
            Self::Delete(request) => build_delete_row_sql(request),
        }
    }
}

#[derive(Debug)]
pub struct QueryResultBatch {
    pub batches: Vec<Arc<RecordBatch>>,
    pub total_count: i64,
}

struct EngineState {
    manager: ConnectionManager,
    postgres_connections: HashMap<ConnectionId, Arc<PostgresConn>>,
    sqlite_connections: HashMap<ConnectionId, Arc<SQLiteConn>>,
    next_session_id: ConnectionId,
}

impl EngineState {
    fn new() -> Self {
        Self {
            manager: ConnectionManager::new(),
            postgres_connections: HashMap::new(),
            sqlite_connections: HashMap::new(),
            next_session_id: 1,
        }
    }
}

#[derive(Clone)]
pub struct Engine {
    state: Arc<RwLock<EngineState>>,
    runtime: tokio::runtime::Handle,
}

impl Engine {
    pub fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(EngineState::new())),
            runtime: RUNTIME.handle().clone(),
        }
    }

    pub fn connect(
        &self,
        uri: String,
        name: String,
    ) -> impl Future<Output = Result<ConnectionId>> + Send + 'static {
        let state = self.state.clone();
        let runtime = self.runtime.clone();
        async move {
            runtime
                .spawn(async move {
                    tokio::time::timeout(
                        Duration::from_secs(OPERATION_TIMEOUT_SECS),
                        connect_inner(state, uri, name),
                    )
                    .await
                    .map_err(|_| DbEngineError::Timeout("Database connection timed out".into()))?
                })
                .await
                .map_err(|error| DbEngineError::Connection(error.to_string()))?
        }
    }

    pub fn disconnect(&self, connection_id: ConnectionId) -> Result<()> {
        let mut state = self
            .state
            .write()
            .map_err(|error| DbEngineError::EngineLock(error.to_string()))?;
        if state.postgres_connections.remove(&connection_id).is_some()
            || state.sqlite_connections.remove(&connection_id).is_some()
            || state.manager.remove_client(connection_id).is_ok()
        {
            Ok(())
        } else {
            Err(DbEngineError::Connection(format!(
                "Connection {connection_id} not found"
            )))
        }
    }

    pub fn execute_sql(
        &self,
        request: QueryRequest,
    ) -> impl Future<Output = Result<QueryResultBatch>> + Send + 'static {
        self.execute_sql_page(request, 0)
    }

    /// Executes a read-only SQL request starting at `offset` rows.
    pub fn execute_sql_page(
        &self,
        request: QueryRequest,
        offset: i64,
    ) -> impl Future<Output = Result<QueryResultBatch>> + Send + 'static {
        let state = self.state.clone();
        let runtime = self.runtime.clone();
        async move {
            let connection_id = request.connection_id;
            let operation_state = state.clone();
            let result = runtime
                .spawn(async move {
                    tokio::time::timeout(
                        Duration::from_secs(OPERATION_TIMEOUT_SECS),
                        execute_sql_inner(operation_state, request, offset),
                    )
                    .await
                })
                .await
                .map_err(|error| DbEngineError::Query(error.to_string()))?;

            match result {
                Ok(result) => result,
                Err(_) => {
                    interrupt_sqlite(&state, connection_id);
                    Err(DbEngineError::Timeout("SQL query timed out".into()))
                }
            }
        }
    }

    pub fn update_cell(
        &self,
        request: UpdateCellRequest,
    ) -> impl Future<Output = Result<u64>> + Send + 'static {
        let state = self.state.clone();
        let runtime = self.runtime.clone();
        async move {
            runtime
                .spawn(async move {
                    tokio::time::timeout(
                        Duration::from_secs(OPERATION_TIMEOUT_SECS),
                        update_cell_inner(state, request),
                    )
                    .await
                    .map_err(|_| DbEngineError::Timeout("Update timed out".into()))?
                })
                .await
                .map_err(|error| DbEngineError::Query(error.to_string()))?
        }
    }

    pub fn apply_cell_updates(
        &self,
        requests: Vec<UpdateCellRequest>,
    ) -> impl Future<Output = Result<u64>> + Send + 'static {
        let state = self.state.clone();
        let runtime = self.runtime.clone();
        async move {
            runtime
                .spawn(async move {
                    tokio::time::timeout(
                        Duration::from_secs(OPERATION_TIMEOUT_SECS),
                        apply_cell_updates_inner(state, requests),
                    )
                    .await
                    .map_err(|_| DbEngineError::Timeout("Update transaction timed out".into()))?
                })
                .await
                .map_err(|error| DbEngineError::Query(error.to_string()))?
        }
    }

    pub fn apply_sql_mutations(
        &self,
        mutations: Vec<SqlMutation>,
    ) -> impl Future<Output = Result<u64>> + Send + 'static {
        let state = self.state.clone();
        let runtime = self.runtime.clone();
        async move {
            runtime
                .spawn(async move { apply_sql_mutations_inner(state, mutations).await })
                .await
                .map_err(|error| DbEngineError::Query(error.to_string()))?
        }
    }

    pub fn delete_row(
        &self,
        request: DeleteRowRequest,
    ) -> impl Future<Output = Result<u64>> + Send + 'static {
        let state = self.state.clone();
        let runtime = self.runtime.clone();
        async move {
            runtime
                .spawn(async move {
                    tokio::time::timeout(
                        Duration::from_secs(OPERATION_TIMEOUT_SECS),
                        delete_row_inner(state, request),
                    )
                    .await
                    .map_err(|_| DbEngineError::Timeout("Delete timed out".into()))?
                })
                .await
                .map_err(|error| DbEngineError::Query(error.to_string()))?
        }
    }

    pub fn insert_row(
        &self,
        request: InsertRowRequest,
    ) -> impl Future<Output = Result<u64>> + Send + 'static {
        let state = self.state.clone();
        let runtime = self.runtime.clone();
        async move {
            runtime
                .spawn(async move {
                    tokio::time::timeout(
                        Duration::from_secs(OPERATION_TIMEOUT_SECS),
                        insert_row_inner(state, request),
                    )
                    .await
                    .map_err(|_| DbEngineError::Timeout("Insert timed out".into()))?
                })
                .await
                .map_err(|error| DbEngineError::Query(error.to_string()))?
        }
    }

    pub fn list_databases(
        &self,
        connection_id: ConnectionId,
    ) -> impl Future<Output = Result<Vec<String>>> + Send + 'static {
        let state = self.state.clone();
        let runtime = self.runtime.clone();
        async move {
            runtime
                .spawn(async move {
                    tokio::time::timeout(
                        Duration::from_secs(OPERATION_TIMEOUT_SECS),
                        list_databases_inner(state, connection_id),
                    )
                    .await
                    .map_err(|_| DbEngineError::Timeout("Catalog load timed out".into()))?
                })
                .await
                .map_err(|error| DbEngineError::Query(error.to_string()))?
        }
    }

    pub fn create_database(
        &self,
        connection_id: ConnectionId,
        name: String,
    ) -> impl Future<Output = Result<()>> + Send + 'static {
        let state = self.state.clone();
        let runtime = self.runtime.clone();
        async move {
            runtime
                .spawn(async move {
                    let postgres = state
                        .read()
                        .map_err(|error| DbEngineError::EngineLock(error.to_string()))?
                        .postgres_connections
                        .get(&connection_id)
                        .cloned()
                        .ok_or_else(|| {
                            DbEngineError::Connection("PostgreSQL connection is unavailable".into())
                        })?;
                    tokio::time::timeout(
                        Duration::from_secs(OPERATION_TIMEOUT_SECS),
                        postgres.create_database(&name),
                    )
                    .await
                    .map_err(|_| {
                        DbEngineError::Timeout(
                            "Create database timed out; server outcome is unknown".into(),
                        )
                    })?
                })
                .await
                .map_err(|error| DbEngineError::Query(error.to_string()))?
        }
    }

    pub fn list_schemas(
        &self,
        connection_id: ConnectionId,
        database: String,
    ) -> impl Future<Output = Result<Vec<String>>> + Send + 'static {
        let state = self.state.clone();
        let runtime = self.runtime.clone();
        async move {
            runtime
                .spawn(async move {
                    tokio::time::timeout(
                        Duration::from_secs(OPERATION_TIMEOUT_SECS),
                        list_schemas_inner(state, connection_id, database),
                    )
                    .await
                    .map_err(|_| DbEngineError::Timeout("Catalog load timed out".into()))?
                })
                .await
                .map_err(|error| DbEngineError::Query(error.to_string()))?
        }
    }

    pub fn list_objects(
        &self,
        connection_id: ConnectionId,
        database: String,
        schema: String,
    ) -> impl Future<Output = Result<Vec<CatalogObject>>> + Send + 'static {
        let state = self.state.clone();
        let runtime = self.runtime.clone();
        async move {
            runtime
                .spawn(async move {
                    tokio::time::timeout(
                        Duration::from_secs(OPERATION_TIMEOUT_SECS),
                        list_objects_inner(state, connection_id, database, schema),
                    )
                    .await
                    .map_err(|_| DbEngineError::Timeout("Catalog load timed out".into()))?
                })
                .await
                .map_err(|error| DbEngineError::Query(error.to_string()))?
        }
    }

    /// Like [`Self::list_objects`] but only objects whose name contains
    /// `filter` (case-insensitive substring; LIKE wildcards are escaped and the
    /// filter is bound as a parameter), at most `limit` rows.
    /// MySQL/MariaDB: not connectable from the app today; add a branch beside
    /// PostgreSQL/SQLite (`name ILIKE`/`LIKE ... ESCAPE`) when they are.
    pub fn list_objects_filtered(
        &self,
        connection_id: ConnectionId,
        database: String,
        schema: String,
        filter: String,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<CatalogObject>>> + Send + 'static {
        let state = self.state.clone();
        let runtime = self.runtime.clone();
        async move {
            runtime
                .spawn(async move {
                    tokio::time::timeout(
                        Duration::from_secs(OPERATION_TIMEOUT_SECS),
                        list_objects_filtered_inner(
                            state,
                            connection_id,
                            database,
                            schema,
                            filter,
                            limit,
                        ),
                    )
                    .await
                    .map_err(|_| DbEngineError::Timeout("Catalog load timed out".into()))?
                })
                .await
                .map_err(|error| DbEngineError::Query(error.to_string()))?
        }
    }

    /// Metadata for the Properties presentation (columns, keys, foreign keys,
    /// indexes, referencing FKs, triggers, DDL). Lists a driver cannot report
    /// are `None`. Contract shared with the GPUI Properties sub-tabs.
    pub fn table_properties(
        &self,
        connection_id: ConnectionId,
        database: String,
        schema: String,
        object: String,
    ) -> impl Future<Output = Result<crate::catalog::TableProperties>> + Send + 'static {
        let state = self.state.clone();
        let runtime = self.runtime.clone();
        async move {
            runtime
                .spawn(async move {
                    tokio::time::timeout(
                        Duration::from_secs(OPERATION_TIMEOUT_SECS),
                        table_properties_inner(state, connection_id, database, schema, object),
                    )
                    .await
                    .map_err(|_| DbEngineError::Timeout("Catalog load timed out".into()))?
                })
                .await
                .map_err(|error| DbEngineError::Query(error.to_string()))?
        }
    }

    pub fn list_columns(
        &self,
        connection_id: ConnectionId,
        database: String,
        schema: String,
        object: String,
    ) -> impl Future<Output = Result<Vec<CatalogColumn>>> + Send + 'static {
        let state = self.state.clone();
        let runtime = self.runtime.clone();
        async move {
            runtime
                .spawn(async move {
                    tokio::time::timeout(
                        Duration::from_secs(OPERATION_TIMEOUT_SECS),
                        list_columns_inner(state, connection_id, database, schema, object),
                    )
                    .await
                    .map_err(|_| DbEngineError::Timeout("Catalog load timed out".into()))?
                })
                .await
                .map_err(|error| DbEngineError::Query(error.to_string()))?
        }
    }

    /// Cancels the in-flight work on a connection. Best effort: SQLite
    /// interrupts the running statement; Postgres sends a server-side cancel.
    /// Unknown connections are ignored rather than erroring, so UI cancel
    /// buttons stay simple.
    pub fn cancel(
        &self,
        connection_id: ConnectionId,
        database: String,
    ) -> impl Future<Output = ()> + Send + 'static {
        let state = self.state.clone();
        let runtime = self.runtime.clone();
        async move {
            let postgres = {
                let Ok(state) = state.read() else {
                    return;
                };
                state.postgres_connections.get(&connection_id).cloned()
            };
            if let Some(postgres) = postgres {
                let database = database.clone();
                let _ = runtime
                    .spawn(async move {
                        postgres.cancel_query(&database).await;
                    })
                    .await;
                return;
            }
            let sqlite = {
                let Ok(state) = state.read() else {
                    return;
                };
                state.sqlite_connections.get(&connection_id).cloned()
            };
            if let Some(sqlite) = sqlite {
                sqlite.interrupt();
            }
        }
    }
}

fn interrupt_sqlite(state: &Arc<RwLock<EngineState>>, connection_id: ConnectionId) {
    if let Ok(state) = state.read() {
        if let Some(sqlite) = state.sqlite_connections.get(&connection_id) {
            sqlite.interrupt();
        }
    }
}

async fn connect_inner(
    state: Arc<RwLock<EngineState>>,
    uri: String,
    name: String,
) -> Result<ConnectionId> {
    let kind = DatabaseKind::from_uri(&uri)?;
    match kind {
        DatabaseKind::Sqlite => {
            let sqlite_uri = uri.clone();
            let conn = tokio::task::spawn_blocking(move || SQLiteConn::connect(&sqlite_uri))
                .await
                .map_err(|error| DbEngineError::Connection(error.to_string()))?
                .map_err(DbEngineError::Connection)?;
            let mut state = state
                .write()
                .map_err(|error| DbEngineError::EngineLock(error.to_string()))?;
            let id = next_connection_id(&mut state);
            state.sqlite_connections.insert(id, Arc::new(conn));
            Ok(id)
        }
        DatabaseKind::Postgres => {
            let conn = PostgresConn::connect(&uri)
                .await
                .map_err(DbEngineError::Connection)?;
            let mut state = state
                .write()
                .map_err(|error| DbEngineError::EngineLock(error.to_string()))?;
            let id = next_connection_id(&mut state);
            state.postgres_connections.insert(id, Arc::new(conn));
            Ok(id)
        }
        DatabaseKind::Mongo => {
            let options = ClientOptions::parse(&uri)
                .await
                .map_err(|error| DbEngineError::Connection(error.to_string()))?;
            let client = Arc::new(
                mongodb::Client::with_options(options)
                    .map_err(|error| DbEngineError::Connection(error.to_string()))?,
            );
            client
                .list_database_names()
                .await
                .map_err(|error| DbEngineError::Connection(error.to_string()))?;

            let mut state = state
                .write()
                .map_err(|error| DbEngineError::EngineLock(error.to_string()))?;
            let id = next_connection_id(&mut state);
            state.manager.insert_client_with_id(id, uri, name, client);
            Ok(id)
        }
    }
}

async fn execute_sql_inner(
    state: Arc<RwLock<EngineState>>,
    request: QueryRequest,
    offset: i64,
) -> Result<QueryResultBatch> {
    let (kind, postgres, sqlite) = {
        let state = state
            .read()
            .map_err(|error| DbEngineError::EngineLock(error.to_string()))?;
        (
            connection_kind(&state, request.connection_id)?,
            state
                .postgres_connections
                .get(&request.connection_id)
                .cloned(),
            state
                .sqlite_connections
                .get(&request.connection_id)
                .cloned(),
        )
    };
    let limit = effective_limit(request.limit);

    match kind {
        DatabaseKind::Postgres => {
            let postgres = postgres.ok_or_else(|| {
                DbEngineError::Connection("Postgres connection storage is missing".into())
            })?;
            let result = postgres
                .execute_sql_page(&request.database, &request.sql, limit, offset)
                .await
                .map_err(DbEngineError::Query)?;
            Ok(result.into())
        }
        DatabaseKind::Sqlite => {
            let sqlite = sqlite.ok_or_else(|| {
                DbEngineError::Connection("SQLite connection storage is missing".into())
            })?;
            let batch = sqlite
                .execute_sql_page(&request.sql, limit, offset)
                .await
                .map_err(DbEngineError::Query)?;
            Ok(QueryResultBatch {
                total_count: batch.num_rows() as i64,
                batches: vec![Arc::new(batch)],
            })
        }
        DatabaseKind::Mongo => Err(DbEngineError::Query(
            "SQL execution is not supported for MongoDB".into(),
        )),
    }
}

async fn update_cell_inner(
    state: Arc<RwLock<EngineState>>,
    request: UpdateCellRequest,
) -> Result<u64> {
    let (kind, postgres, sqlite) = snapshot_connection(&state, request.connection_id)?;
    let sql = build_update_cell_sql(&request).map_err(DbEngineError::Query)?;
    match kind {
        DatabaseKind::Postgres => postgres
            .expect("checked by snapshot_connection")
            .execute_mutation(&request.database, &sql)
            .await
            .map_err(DbEngineError::Query),
        DatabaseKind::Sqlite => sqlite
            .expect("checked by snapshot_connection")
            .execute_mutation(&sql)
            .await
            .map_err(DbEngineError::Query),
        DatabaseKind::Mongo => Err(DbEngineError::Query(
            "Cell updates are not supported for MongoDB".into(),
        )),
    }
}

async fn apply_cell_updates_inner(
    state: Arc<RwLock<EngineState>>,
    requests: Vec<UpdateCellRequest>,
) -> Result<u64> {
    let Some(first) = requests.first() else {
        return Ok(0);
    };
    if requests.iter().any(|request| {
        request.connection_id != first.connection_id || request.database != first.database
    }) {
        return Err(DbEngineError::Query(
            "All cell updates must use the same connection and database".into(),
        ));
    }
    let sql = requests
        .iter()
        .map(build_update_cell_sql)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(DbEngineError::Query)?;
    let (kind, postgres, sqlite) = snapshot_connection(&state, first.connection_id)?;
    match kind {
        DatabaseKind::Postgres => postgres
            .expect("checked by snapshot_connection")
            .execute_mutations(&first.database, &sql)
            .await
            .map_err(DbEngineError::Query),
        DatabaseKind::Sqlite => sqlite
            .expect("checked by snapshot_connection")
            .execute_mutations(&sql)
            .await
            .map_err(DbEngineError::Query),
        DatabaseKind::Mongo => Err(DbEngineError::Query(
            "Cell updates are not supported for MongoDB".into(),
        )),
    }
}

async fn apply_sql_mutations_inner(
    state: Arc<RwLock<EngineState>>,
    mutations: Vec<SqlMutation>,
) -> Result<u64> {
    let Some(first) = mutations.first() else {
        return Ok(0);
    };
    let target = first.target();
    if mutations.iter().any(|mutation| mutation.target() != target) {
        return Err(DbEngineError::Query(
            "SQL mutations must target one connection, database, schema and table".into(),
        ));
    }
    let sql = mutations
        .iter()
        .map(SqlMutation::sql)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(DbEngineError::Query)?;
    let (kind, postgres, sqlite) = snapshot_connection(&state, target.0)?;
    match kind {
        DatabaseKind::Postgres => postgres
            .expect("checked by snapshot_connection")
            .execute_mutations_checked(target.1, &sql, true)
            .await
            .map_err(DbEngineError::Query),
        DatabaseKind::Sqlite => sqlite
            .expect("checked by snapshot_connection")
            .execute_mutations_checked(&sql, true)
            .await
            .map_err(DbEngineError::Query),
        DatabaseKind::Mongo => Err(DbEngineError::Query(
            "SQL batch mutation is not supported for MongoDB".into(),
        )),
    }
}

async fn delete_row_inner(
    state: Arc<RwLock<EngineState>>,
    request: DeleteRowRequest,
) -> Result<u64> {
    let (kind, postgres, sqlite) = snapshot_connection(&state, request.connection_id)?;
    let sql = build_delete_row_sql(&request).map_err(DbEngineError::Query)?;
    match kind {
        DatabaseKind::Postgres => postgres
            .expect("checked by snapshot_connection")
            .execute_mutation(&request.database, &sql)
            .await
            .map_err(DbEngineError::Query),
        DatabaseKind::Sqlite => sqlite
            .expect("checked by snapshot_connection")
            .execute_mutation(&sql)
            .await
            .map_err(DbEngineError::Query),
        DatabaseKind::Mongo => Err(DbEngineError::Query(
            "Row deletion is not supported for MongoDB".into(),
        )),
    }
}

async fn insert_row_inner(
    state: Arc<RwLock<EngineState>>,
    request: InsertRowRequest,
) -> Result<u64> {
    let (kind, postgres, sqlite) = snapshot_connection(&state, request.connection_id)?;
    let sql = build_insert_row_sql(&request).map_err(DbEngineError::Query)?;
    match kind {
        DatabaseKind::Postgres => postgres
            .expect("checked by snapshot_connection")
            .execute_mutation(&request.database, &sql)
            .await
            .map_err(DbEngineError::Query),
        DatabaseKind::Sqlite => sqlite
            .expect("checked by snapshot_connection")
            .execute_mutation(&sql)
            .await
            .map_err(DbEngineError::Query),
        DatabaseKind::Mongo => Err(DbEngineError::Query(
            "Row insertion is not supported for MongoDB".into(),
        )),
    }
}

fn build_update_cell_sql(request: &UpdateCellRequest) -> std::result::Result<String, String> {
    let schema = quote_identifier(&request.schema)?;
    let table = quote_identifier(&request.table)?;
    let column = quote_identifier(&request.column)?;
    let key_column = quote_identifier(&request.key_column)?;
    let value = quote_mutation_value(&request.value)?;
    let key_value = quote_mutation_value(&request.key_value)?;
    Ok(format!(
        "UPDATE {schema}.{table} SET {column} = {value} WHERE {key_column} = {key_value}"
    ))
}

fn build_delete_row_sql(request: &DeleteRowRequest) -> std::result::Result<String, String> {
    let schema = quote_identifier(&request.schema)?;
    let table = quote_identifier(&request.table)?;
    let key_column = quote_identifier(&request.key_column)?;
    let key_value = quote_mutation_value(&request.key_value)?;
    Ok(format!(
        "DELETE FROM {schema}.{table} WHERE {key_column} = {key_value}"
    ))
}

fn build_insert_row_sql(request: &InsertRowRequest) -> std::result::Result<String, String> {
    let schema = quote_identifier(&request.schema)?;
    let table = quote_identifier(&request.table)?;
    if request.values.is_empty() {
        return Ok(format!("INSERT INTO {schema}.{table} DEFAULT VALUES"));
    }
    let columns = request
        .values
        .iter()
        .map(|(column, _)| quote_identifier(column))
        .collect::<std::result::Result<Vec<_>, _>>()?
        .join(", ");
    let values = request
        .values
        .iter()
        .map(|(_, value)| quote_mutation_value(value))
        .collect::<std::result::Result<Vec<_>, _>>()?
        .join(", ");
    Ok(format!(
        "INSERT INTO {schema}.{table} ({columns}) VALUES ({values})"
    ))
}

fn quote_identifier(value: &str) -> std::result::Result<String, String> {
    if value.is_empty() || value.contains('\0') {
        return Err("SQL identifier cannot be empty or contain NUL".into());
    }
    Ok(format!("\"{}\"", value.replace('"', "\"\"")))
}

fn quote_mutation_value(value: &MutationValue) -> std::result::Result<String, String> {
    match value {
        MutationValue::Null => Ok("NULL".into()),
        MutationValue::Text(value) => Ok(format!("'{}'", value.replace('\'', "''"))),
        MutationValue::Boolean(value) => Ok(if *value { "TRUE" } else { "FALSE" }.into()),
        MutationValue::Number(value) => {
            let parsed = value
                .parse::<f64>()
                .map_err(|_| "Mutation number is invalid".to_string())?;
            if !parsed.is_finite() {
                return Err("Mutation number must be finite".into());
            }
            Ok(value.clone())
        }
    }
}

async fn list_databases_inner(
    state: Arc<RwLock<EngineState>>,
    connection_id: ConnectionId,
) -> Result<Vec<String>> {
    let (kind, postgres, sqlite) = snapshot_connection(&state, connection_id)?;
    match kind {
        DatabaseKind::Postgres => {
            let postgres = postgres.expect("checked by snapshot_connection");
            Ok(postgres
                .list_databases()
                .await
                .map_err(DbEngineError::Query)?)
        }
        DatabaseKind::Sqlite => {
            let sqlite = sqlite.expect("checked by snapshot_connection");
            Ok(sqlite
                .list_databases()
                .await
                .map_err(DbEngineError::Query)?)
        }
        DatabaseKind::Mongo => {
            let client = mongo_client(&state, connection_id)?;
            Ok(client
                .list_database_names()
                .await
                .map_err(|error| DbEngineError::Query(error.to_string()))?)
        }
    }
}

async fn list_schemas_inner(
    state: Arc<RwLock<EngineState>>,
    connection_id: ConnectionId,
    database: String,
) -> Result<Vec<String>> {
    let (kind, postgres, sqlite) = snapshot_connection(&state, connection_id)?;
    match kind {
        DatabaseKind::Postgres => {
            let postgres = postgres.expect("checked by snapshot_connection");
            Ok(postgres
                .list_schemas(&database)
                .await
                .map_err(DbEngineError::Query)?)
        }
        DatabaseKind::Sqlite => {
            let sqlite = sqlite.expect("checked by snapshot_connection");
            Ok(sqlite
                .list_databases()
                .await
                .map_err(DbEngineError::Query)?)
        }
        DatabaseKind::Mongo => Err(DbEngineError::Query(
            "Schemas are not supported for MongoDB".into(),
        )),
    }
}

async fn list_objects_inner(
    state: Arc<RwLock<EngineState>>,
    connection_id: ConnectionId,
    database: String,
    schema: String,
) -> Result<Vec<CatalogObject>> {
    let (kind, postgres, sqlite) = snapshot_connection(&state, connection_id)?;
    match kind {
        DatabaseKind::Postgres => {
            let postgres = postgres.expect("checked by snapshot_connection");
            let objects = postgres
                .list_sql_objects(&database, &schema)
                .await
                .map_err(DbEngineError::Query)?;
            Ok(objects
                .into_iter()
                .map(|object| CatalogObject {
                    name: object.name,
                    kind: match object.kind {
                        crate::db::postgres::SqlObjectKind::Table => CatalogObjectKind::Table,
                        crate::db::postgres::SqlObjectKind::View => CatalogObjectKind::View,
                        crate::db::postgres::SqlObjectKind::MaterializedView => {
                            CatalogObjectKind::MaterializedView
                        }
                    },
                })
                .collect())
        }
        DatabaseKind::Sqlite => {
            let sqlite = sqlite.expect("checked by snapshot_connection");
            Ok(sqlite
                .list_objects(&schema)
                .await
                .map_err(DbEngineError::Query)?)
        }
        DatabaseKind::Mongo => Err(DbEngineError::Query(
            "SQL object browsing is not supported for MongoDB".into(),
        )),
    }
}

async fn list_objects_filtered_inner(
    state: Arc<RwLock<EngineState>>,
    connection_id: ConnectionId,
    database: String,
    schema: String,
    filter: String,
    limit: usize,
) -> Result<Vec<CatalogObject>> {
    let (kind, postgres, sqlite) = snapshot_connection(&state, connection_id)?;
    match kind {
        DatabaseKind::Postgres => {
            let postgres = postgres.expect("checked by snapshot_connection");
            let objects = postgres
                .list_sql_objects_filtered(&database, &schema, &filter, limit)
                .await
                .map_err(DbEngineError::Query)?;
            Ok(objects
                .into_iter()
                .map(|object| CatalogObject {
                    name: object.name,
                    kind: match object.kind {
                        crate::db::postgres::SqlObjectKind::Table => CatalogObjectKind::Table,
                        crate::db::postgres::SqlObjectKind::View => CatalogObjectKind::View,
                        crate::db::postgres::SqlObjectKind::MaterializedView => {
                            CatalogObjectKind::MaterializedView
                        }
                    },
                })
                .collect())
        }
        DatabaseKind::Sqlite => {
            let sqlite = sqlite.expect("checked by snapshot_connection");
            Ok(sqlite
                .list_objects_filtered(&schema, &filter, limit)
                .await
                .map_err(DbEngineError::Query)?)
        }
        // MongoDB collections are listed through the driver provider, not here.
        DatabaseKind::Mongo => Err(DbEngineError::Query(
            "SQL object browsing is not supported for MongoDB".into(),
        )),
    }
}

async fn list_columns_inner(
    state: Arc<RwLock<EngineState>>,
    connection_id: ConnectionId,
    database: String,
    schema: String,
    object: String,
) -> Result<Vec<CatalogColumn>> {
    let (kind, postgres, sqlite) = snapshot_connection(&state, connection_id)?;
    match kind {
        DatabaseKind::Postgres => {
            let postgres = postgres.expect("checked by snapshot_connection");
            Ok(postgres
                .list_columns(&database, &schema, &object)
                .await
                .map_err(DbEngineError::Query)?
                .into_iter()
                .map(|column| CatalogColumn {
                    name: column.name,
                    data_type: column.data_type,
                    nullable: column.nullable,
                    primary_key: column.primary_key,
                    is_generated: column.is_generated,
                    default_expression: column.default_expression,
                    auto_increment: column.auto_increment,
                    comment: column.comment,
                    attributes: None,
                })
                .collect())
        }
        DatabaseKind::Sqlite => {
            let sqlite = sqlite.expect("checked by snapshot_connection");
            Ok(sqlite
                .list_columns(&schema, &object)
                .await
                .map_err(DbEngineError::Query)?)
        }
        DatabaseKind::Mongo => Err(DbEngineError::Query(
            "SQL column browsing is not supported for MongoDB".into(),
        )),
    }
}

async fn table_properties_inner(
    state: Arc<RwLock<EngineState>>,
    connection_id: ConnectionId,
    database: String,
    schema: String,
    object: String,
) -> Result<crate::catalog::TableProperties> {
    let (kind, postgres, sqlite) = snapshot_connection(&state, connection_id)?;
    match kind {
        DatabaseKind::Postgres => {
            let postgres = postgres.expect("checked by snapshot_connection");
            postgres
                .table_properties(&database, &schema, &object)
                .await
                .map(postgres_properties_to_catalog)
                .map_err(DbEngineError::Query)
        }
        DatabaseKind::Sqlite => {
            let sqlite = sqlite.expect("checked by snapshot_connection");
            sqlite
                .table_properties(&schema, &object)
                .await
                .map_err(DbEngineError::Query)
        }
        // Same stance as `list_columns`: SQL catalog browsing is not offered
        // for MongoDB (collections go through the driver provider).
        DatabaseKind::Mongo => Err(DbEngineError::Query(
            "SQL table properties are not supported for MongoDB".into(),
        )),
    }
}

fn postgres_properties_to_catalog(
    properties: crate::db::postgres::SqlTableProperties,
) -> crate::catalog::TableProperties {
    use crate::catalog::{
        CatalogConstraint, CatalogConstraintKind, CatalogForeignKey, CatalogIndex, CatalogTrigger,
    };
    let foreign_key = |fk: crate::db::postgres::SqlForeignKey| CatalogForeignKey {
        name: fk.name,
        table: fk.table,
        columns: fk.columns,
        referenced_table: fk.referenced_table,
        referenced_columns: fk.referenced_columns,
        on_update: Some(fk.on_update),
        on_delete: Some(fk.on_delete),
        definition: Some(fk.definition),
    };
    crate::catalog::TableProperties {
        columns: properties
            .columns
            .into_iter()
            .map(|column| CatalogColumn {
                name: column.name,
                data_type: column.data_type,
                nullable: column.nullable,
                primary_key: column.primary_key,
                is_generated: column.is_generated,
                default_expression: column.default_expression,
                auto_increment: column.auto_increment,
                comment: column.comment,
                attributes: None,
            })
            .collect(),
        keys: Some(
            properties
                .constraints
                .into_iter()
                .map(|constraint| CatalogConstraint {
                    name: constraint.name,
                    kind: match constraint.kind_code.as_str() {
                        "p" => CatalogConstraintKind::PrimaryKey,
                        "u" => CatalogConstraintKind::Unique,
                        "c" => CatalogConstraintKind::Check,
                        "x" => CatalogConstraintKind::Exclusion,
                        _ => CatalogConstraintKind::Other,
                    },
                    columns: constraint.columns,
                    definition: Some(constraint.definition),
                })
                .collect(),
        ),
        foreign_keys: Some(properties.foreign_keys.into_iter().map(foreign_key).collect()),
        indexes: Some(
            properties
                .indexes
                .into_iter()
                .map(|index| CatalogIndex {
                    name: index.name,
                    columns: index.columns,
                    unique: index.unique,
                    primary: index.primary,
                    method: Some(index.method),
                    definition: Some(index.definition),
                })
                .collect(),
        ),
        references: Some(properties.references.into_iter().map(foreign_key).collect()),
        triggers: Some(
            properties
                .triggers
                .into_iter()
                .map(|trigger| CatalogTrigger {
                    name: trigger.name,
                    timing: Some(trigger.timing),
                    events: trigger.events,
                    enabled: Some(trigger.enabled),
                    definition: Some(trigger.definition),
                })
                .collect(),
        ),
        ddl: Some(properties.ddl),
    }
}

/// Snapshots connection handles under the read lock and releases the lock
/// before any await, so catalog loads never block connect/disconnect.
fn snapshot_connection(
    state: &Arc<RwLock<EngineState>>,
    connection_id: ConnectionId,
) -> Result<(
    DatabaseKind,
    Option<Arc<PostgresConn>>,
    Option<Arc<SQLiteConn>>,
)> {
    let state = state
        .read()
        .map_err(|error| DbEngineError::EngineLock(error.to_string()))?;
    let kind = connection_kind(&state, connection_id)?;
    let postgres = state.postgres_connections.get(&connection_id).cloned();
    let sqlite = state.sqlite_connections.get(&connection_id).cloned();
    Ok((kind, postgres, sqlite))
}

fn mongo_client(
    state: &Arc<RwLock<EngineState>>,
    connection_id: ConnectionId,
) -> Result<Arc<mongodb::Client>> {
    let state = state
        .read()
        .map_err(|error| DbEngineError::EngineLock(error.to_string()))?;
    state
        .manager
        .get_client(connection_id)
        .map_err(|error| DbEngineError::Connection(error.to_string()))
}

impl From<QueryResult> for QueryResultBatch {
    fn from(result: QueryResult) -> Self {
        let total_count = result.total_count;
        Self {
            batches: result.batches.into_iter().map(Arc::new).collect(),
            total_count,
        }
    }
}

fn next_connection_id(state: &mut EngineState) -> ConnectionId {
    let id = state.next_session_id;
    state.next_session_id += 1;
    id
}

fn connection_kind(state: &EngineState, connection_id: ConnectionId) -> Result<DatabaseKind> {
    if state.postgres_connections.contains_key(&connection_id) {
        Ok(DatabaseKind::Postgres)
    } else if state.sqlite_connections.contains_key(&connection_id) {
        Ok(DatabaseKind::Sqlite)
    } else if state.manager.get_client(connection_id).is_ok() {
        Ok(DatabaseKind::Mongo)
    } else {
        Err(DbEngineError::Connection(format!(
            "Connection {connection_id} not found"
        )))
    }
}

fn effective_limit(limit: i64) -> i64 {
    if limit > 0 {
        limit
    } else {
        200
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_update_cell_sql, effective_limit, DatabaseKind, DeleteRowRequest, Engine,
        InsertRowRequest, MutationValue, UpdateCellRequest,
    };
    use arrow_array::{Int64Array, StringArray};
    use std::sync::Arc;

    #[test]
    fn detects_supported_database_kind_from_uri() {
        assert_eq!(
            DatabaseKind::from_uri("sqlite:///tmp/table.db").expect("supported URI"),
            DatabaseKind::Sqlite
        );
        assert_eq!(
            DatabaseKind::from_uri("postgresql://localhost/db").expect("supported URI"),
            DatabaseKind::Postgres
        );
        assert_eq!(
            DatabaseKind::from_uri("mongodb://localhost").expect("supported URI"),
            DatabaseKind::Mongo
        );
    }

    #[test]
    fn rejects_unknown_database_uri_schemes() {
        assert!(DatabaseKind::from_uri("mysql://localhost/db").is_err());
        assert!(DatabaseKind::from_uri("not-a-database").is_err());
    }

    #[test]
    fn unsupported_scheme_error_does_not_leak_the_uri() {
        let error = DatabaseKind::from_uri("postgres://user:secret@host/db")
            .expect("postgres scheme is supported");
        let _ = error;

        let message = DatabaseKind::from_uri("mysql://user:secret@host/db")
            .expect_err("mysql is unsupported")
            .to_string();
        assert!(
            !message.contains("secret"),
            "error must not echo credentials: {message}"
        );
        assert!(
            !message.contains("mysql://"),
            "error must not echo the URI: {message}"
        );
    }

    #[test]
    fn normalizes_non_positive_limits_to_the_default() {
        assert_eq!(effective_limit(100), 100);
        assert_eq!(effective_limit(0), 200);
        assert_eq!(effective_limit(-1), 200);
    }

    #[test]
    fn builds_update_sql_with_quoted_identifiers_and_values() {
        let request = UpdateCellRequest {
            connection_id: 1,
            database: "app".into(),
            schema: "public".into(),
            table: "users".into(),
            key_column: "id".into(),
            key_value: MutationValue::Number("7".into()),
            column: "name".into(),
            value: MutationValue::Text("O'Brien".into()),
        };

        assert_eq!(
            build_update_cell_sql(&request).expect("valid update"),
            "UPDATE \"public\".\"users\" SET \"name\" = 'O''Brien' WHERE \"id\" = 7"
        );
    }

    #[test]
    fn rejects_untrusted_numeric_literals() {
        let request = UpdateCellRequest {
            connection_id: 1,
            database: "app".into(),
            schema: "public".into(),
            table: "users".into(),
            key_column: "id".into(),
            key_value: MutationValue::Number("7 OR 1=1".into()),
            column: "name".into(),
            value: MutationValue::Null,
        };

        assert!(build_update_cell_sql(&request).is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn updates_one_sqlite_cell_without_accepting_raw_write_sql() {
        let engine = Engine::new();
        let connection_id = engine
            .connect("sqlite://:memory:".to_string(), "mutation".to_string())
            .await
            .expect("SQLite connection");
        engine
            .execute_sql(super::QueryRequest {
                connection_id,
                database: String::new(),
                sql: "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)".into(),
                limit: 0,
            })
            .await
            .expect("create table");
        engine
            .execute_sql(super::QueryRequest {
                connection_id,
                database: String::new(),
                sql: "INSERT INTO users VALUES (1, 'Ada')".into(),
                limit: 0,
            })
            .await
            .expect("insert row");

        let changed = engine
            .update_cell(UpdateCellRequest {
                connection_id,
                database: String::new(),
                schema: "main".into(),
                table: "users".into(),
                key_column: "id".into(),
                key_value: MutationValue::Number("1".into()),
                column: "name".into(),
                value: MutationValue::Text("Grace".into()),
            })
            .await
            .expect("update cell");
        assert_eq!(changed, 1);

        let result = engine
            .execute_sql(super::QueryRequest {
                connection_id,
                database: String::new(),
                sql: "SELECT name FROM users".into(),
                limit: 0,
            })
            .await
            .expect("read updated row");
        let name = result.batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("text result column");
        assert_eq!(name.value(0), "Grace");
        engine.disconnect(connection_id).expect("disconnect");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn create_database_refuses_sqlite_and_missing_connection_without_server_write() {
        let engine = Engine::new();
        let sqlite = engine
            .connect("sqlite://:memory:".into(), "fixture".into())
            .await
            .unwrap();
        assert!(engine
            .create_database(sqlite, "unsafe".into())
            .await
            .is_err());
        assert!(engine
            .create_database(u64::MAX, "unsafe".into())
            .await
            .is_err());
        engine.disconnect(sqlite).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rolls_back_all_sqlite_cell_updates_when_one_update_fails() {
        let engine = Engine::new();
        let connection_id = engine
            .connect(
                "sqlite://:memory:".to_string(),
                "mutation-batch".to_string(),
            )
            .await
            .expect("SQLite connection");
        for sql in [
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)",
            "INSERT INTO users VALUES (1, 'Ada'), (2, 'Grace')",
        ] {
            engine
                .execute_sql(super::QueryRequest {
                    connection_id,
                    database: String::new(),
                    sql: sql.into(),
                    limit: 0,
                })
                .await
                .expect("seed statement");
        }

        let mut first = UpdateCellRequest {
            connection_id,
            database: String::new(),
            schema: "main".into(),
            table: "users".into(),
            key_column: "id".into(),
            key_value: MutationValue::Number("1".into()),
            column: "name".into(),
            value: MutationValue::Text("Changed".into()),
        };
        let mut second = first.clone();
        second.key_value = MutationValue::Number("2".into());
        second.column = "missing".into();
        let result = engine.apply_cell_updates(vec![first.clone(), second]);
        assert!(result.await.is_err());

        first.value = MutationValue::Text("Unused".into());
        let rows = engine
            .execute_sql(super::QueryRequest {
                connection_id,
                database: String::new(),
                sql: "SELECT name FROM users ORDER BY id".into(),
                limit: 0,
            })
            .await
            .expect("read unchanged rows");
        let names = rows.batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("text result column");
        assert_eq!(names.value(0), "Ada");
        assert_eq!(names.value(1), "Grace");
        engine.disconnect(connection_id).expect("disconnect");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mixed_sql_mutations_roll_back_every_statement_on_constraint_failure() {
        let engine = Engine::new();
        let connection_id = engine
            .connect("sqlite://:memory:".into(), "mixed".into())
            .await
            .unwrap();
        for sql in [
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL)",
            "INSERT INTO users VALUES (1, 'Ada'), (2, 'Grace')",
        ] {
            engine
                .execute_sql(super::QueryRequest {
                    connection_id,
                    database: String::new(),
                    sql: sql.into(),
                    limit: 0,
                })
                .await
                .unwrap();
        }
        let result = engine
            .apply_sql_mutations(vec![
                super::SqlMutation::Update(UpdateCellRequest {
                    connection_id,
                    database: String::new(),
                    schema: "main".into(),
                    table: "users".into(),
                    key_column: "id".into(),
                    key_value: MutationValue::Number("1".into()),
                    column: "name".into(),
                    value: MutationValue::Text("Changed".into()),
                }),
                super::SqlMutation::Delete(DeleteRowRequest {
                    connection_id,
                    database: String::new(),
                    schema: "main".into(),
                    table: "users".into(),
                    key_column: "id".into(),
                    key_value: MutationValue::Number("1".into()),
                }),
                super::SqlMutation::Insert(InsertRowRequest {
                    connection_id,
                    database: String::new(),
                    schema: "main".into(),
                    table: "users".into(),
                    values: vec![
                        ("id".into(), MutationValue::Number("2".into())),
                        ("name".into(), MutationValue::Text("duplicate".into())),
                    ],
                }),
            ])
            .await;
        assert!(result.is_err());
        let rows = engine
            .execute_sql(super::QueryRequest {
                connection_id,
                database: String::new(),
                sql: "SELECT name FROM users ORDER BY id".into(),
                limit: 0,
            })
            .await
            .unwrap();
        let names = rows.batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!((names.value(0), names.value(1)), ("Ada", "Grace"));
        engine.disconnect(connection_id).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mixed_sql_mutations_commit_all_rows_in_one_sqlite_transaction() {
        let engine = Engine::new();
        let connection_id = engine
            .connect("sqlite://:memory:".into(), "mixed-success".into())
            .await
            .unwrap();
        for sql in [
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL)",
            "INSERT INTO users VALUES (1, 'Ada'), (2, 'Grace')",
        ] {
            engine
                .execute_sql(super::QueryRequest {
                    connection_id,
                    database: String::new(),
                    sql: sql.into(),
                    limit: 0,
                })
                .await
                .unwrap();
        }
        let target = (
            connection_id,
            String::new(),
            "main".to_string(),
            "users".to_string(),
        );
        let affected = engine
            .apply_sql_mutations(vec![
                super::SqlMutation::Update(UpdateCellRequest {
                    connection_id: target.0,
                    database: target.1.clone(),
                    schema: target.2.clone(),
                    table: target.3.clone(),
                    key_column: "id".into(),
                    key_value: MutationValue::Number("1".into()),
                    column: "name".into(),
                    value: MutationValue::Text("Updated".into()),
                }),
                super::SqlMutation::Insert(InsertRowRequest {
                    connection_id: target.0,
                    database: target.1.clone(),
                    schema: target.2.clone(),
                    table: target.3.clone(),
                    values: vec![
                        ("id".into(), MutationValue::Number("3".into())),
                        ("name".into(), MutationValue::Text("New".into())),
                    ],
                }),
                super::SqlMutation::Delete(DeleteRowRequest {
                    connection_id: target.0,
                    database: target.1,
                    schema: target.2,
                    table: target.3,
                    key_column: "id".into(),
                    key_value: MutationValue::Number("2".into()),
                }),
            ])
            .await
            .unwrap();
        assert_eq!(affected, 3);
        let rows = engine
            .execute_sql(super::QueryRequest {
                connection_id,
                database: String::new(),
                sql: "SELECT name FROM users ORDER BY id".into(),
                limit: 0,
            })
            .await
            .unwrap();
        let names = rows.batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!((names.value(0), names.value(1)), ("Updated", "New"));
        engine.disconnect(connection_id).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mixed_postgres_mutations_roll_back_on_constraint_error_in_owned_temp_cluster() {
        let (Ok(uri), Ok(expected_dir)) = (
            std::env::var("TABLEX_TEST_OWNED_PG_URI"),
            std::env::var("TABLEX_TEST_OWNED_PG_DATA_DIR"),
        ) else { return };
        let approved = std::env::temp_dir().join("opencode");
        let expected = std::fs::canonicalize(&expected_dir).unwrap();
        assert!(expected.starts_with(std::fs::canonicalize(approved).unwrap()));
        let engine = Engine::new();
        let connection_id = engine.connect(uri.clone(), "isolated-pg".into()).await.unwrap();
        let verify = engine.execute_sql(super::QueryRequest {
            connection_id,
            database: String::new(),
            sql: "SELECT current_setting('data_directory')".into(),
            limit: 0,
        }).await.unwrap();
        let server_dir = verify.batches[0].column(0).as_any().downcast_ref::<StringArray>().unwrap().value(0);
        assert_eq!(std::fs::canonicalize(server_dir).unwrap(), expected);
        let (client, connection) = tokio_postgres::connect(&uri, tokio_postgres::NoTls).await.unwrap();
        tokio::spawn(async move { connection.await.unwrap(); });
        client.batch_execute("CREATE TABLE tablex_batch_test (id BIGINT PRIMARY KEY, name TEXT NOT NULL); INSERT INTO tablex_batch_test VALUES (1, 'Ada'), (2, 'Grace')").await.unwrap();
        let result = engine.apply_sql_mutations(vec![
            super::SqlMutation::Update(UpdateCellRequest {
                connection_id, database: String::new(), schema: "public".into(), table: "tablex_batch_test".into(),
                key_column: "id".into(), key_value: MutationValue::Number("1".into()),
                column: "name".into(), value: MutationValue::Text("Changed".into()),
            }),
            super::SqlMutation::Insert(InsertRowRequest {
                connection_id, database: String::new(), schema: "public".into(), table: "tablex_batch_test".into(),
                values: vec![("id".into(), MutationValue::Number("2".into())), ("name".into(), MutationValue::Text("Duplicate".into()))],
            }),
        ]).await;
        assert!(result.is_err());
        let rows = engine.execute_sql(super::QueryRequest {
            connection_id, database: String::new(),
            sql: "SELECT name FROM tablex_batch_test ORDER BY id".into(), limit: 0,
        }).await.unwrap();
        let names = rows.batches[0].column(0).as_any().downcast_ref::<StringArray>().unwrap();
        assert_eq!((names.value(0), names.value(1)), ("Ada", "Grace"));
        engine.disconnect(connection_id).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn postgres_catalog_reports_identity_serial_generated_and_pk_in_owned_temp_cluster() {
        let (Ok(uri), Ok(expected_dir)) = (
            std::env::var("TABLEX_TEST_OWNED_PG_URI"),
            std::env::var("TABLEX_TEST_OWNED_PG_DATA_DIR"),
        ) else { return };
        let approved = std::fs::canonicalize(std::env::temp_dir().join("opencode")).unwrap();
        let expected = std::fs::canonicalize(expected_dir).unwrap();
        assert!(expected.starts_with(approved));
        let engine = Engine::new();
        let id = engine.connect(uri.clone(), "catalog-pg".into()).await.unwrap();
        let verify = engine.execute_sql(super::QueryRequest {
            connection_id: id, database: String::new(),
            sql: "SELECT current_setting('data_directory')".into(), limit: 0,
        }).await.unwrap();
        let actual = verify.batches[0].column(0).as_any().downcast_ref::<StringArray>().unwrap().value(0);
        assert_eq!(std::fs::canonicalize(actual).unwrap(), expected);
        let (client, connection) = tokio_postgres::connect(&uri, tokio_postgres::NoTls).await.unwrap();
        tokio::spawn(async move { connection.await.unwrap(); });
        client.batch_execute("CREATE TABLE tablex_catalog_test (id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY, name TEXT NOT NULL, slug TEXT GENERATED ALWAYS AS (lower(name)) STORED, serial_id BIGSERIAL)").await.unwrap();
        let columns = engine.list_columns(id, String::new(), "public".into(), "tablex_catalog_test".into()).await.unwrap();
        assert_eq!(columns.len(), 4);
        assert!(columns[0].primary_key);
        assert_eq!(columns[0].auto_increment, Some(true));
        assert_eq!(columns[1].is_generated, Some(false));
        assert_eq!(columns[2].is_generated, Some(true));
        assert_eq!(columns[3].auto_increment, Some(true));
        let applied = engine.apply_sql_mutations(vec![super::SqlMutation::Insert(InsertRowRequest {
            connection_id: id, database: String::new(), schema: "public".into(), table: "tablex_catalog_test".into(),
            values: vec![("name".into(), MutationValue::Text("Ada".into()))],
        })]).await.unwrap();
        assert_eq!(applied, 1);
        let rows = engine.execute_sql(super::QueryRequest {
            connection_id: id, database: String::new(), sql: "SELECT id, slug, serial_id FROM tablex_catalog_test".into(), limit: 0,
        }).await.unwrap();
        assert_eq!(rows.batches[0].num_rows(), 1);
        assert_eq!(rows.batches[0].column(1).as_any().downcast_ref::<StringArray>().unwrap().value(0), "ada");
        engine.disconnect(id).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sqlite_catalog_marks_generated_defaults_and_rowid_identity_for_safe_clone() {
        let engine = Engine::new();
        let id = engine
            .connect("sqlite://:memory:".into(), "catalog-generation".into())
            .await
            .unwrap();
        engine
            .execute_sql(super::QueryRequest {
                connection_id: id,
                database: String::new(),
                sql: "CREATE TABLE orders (id INTEGER PRIMARY KEY, name TEXT NOT NULL DEFAULT 'guest', slug TEXT GENERATED ALWAYS AS (lower(name)) STORED)".into(),
                limit: 0,
            })
            .await
            .unwrap();
        let columns = engine
            .list_columns(id, String::new(), "main".into(), "orders".into())
            .await
            .unwrap();
        assert_eq!(columns.len(), 3);
        assert_eq!(columns[0].auto_increment, Some(true));
        assert_eq!(columns[1].default_expression.as_deref(), Some("'guest'"));
        assert_eq!(columns[2].is_generated, Some(true));
        let insert = super::SqlMutation::Insert(InsertRowRequest {
            connection_id: id,
            database: String::new(),
            schema: "main".into(),
            table: "orders".into(),
            values: vec![("name".into(), MutationValue::Text("Ada".into()))],
        });
        assert_eq!(
            engine
                .apply_sql_mutations(vec![insert.clone(), insert])
                .await
                .unwrap(),
            2
        );
        let rows = engine
            .execute_sql(super::QueryRequest {
                connection_id: id,
                database: String::new(),
                sql: "SELECT id, slug FROM orders ORDER BY id".into(),
                limit: 0,
            })
            .await
            .unwrap();
        let ids = rows.batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        let slugs = rows.batches[0]
            .column(1)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!((ids.value(0), ids.value(1)), (1, 2));
        assert_eq!((slugs.value(0), slugs.value(1)), ("ada", "ada"));
        for sql in [
            "CREATE TABLE manual_key (id INTEGER PRIMARY KEY, name TEXT) WITHOUT ROWID",
            "CREATE TABLE composite_key (id INTEGER, tenant TEXT, PRIMARY KEY (id, tenant))",
        ] {
            engine
                .execute_sql(super::QueryRequest {
                    connection_id: id,
                    database: String::new(),
                    sql: sql.into(),
                    limit: 0,
                })
                .await
                .unwrap();
        }
        let manual = engine
            .list_columns(id, String::new(), "main".into(), "manual_key".into())
            .await
            .unwrap();
        assert_eq!(manual[0].auto_increment, Some(false));
        let composite = engine
            .list_columns(id, String::new(), "main".into(), "composite_key".into())
            .await
            .unwrap();
        assert_eq!(
            composite.iter().filter(|column| column.primary_key).count(),
            2
        );
        assert!(composite
            .iter()
            .all(|column| column.auto_increment == Some(false)));
        engine.disconnect(id).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn deletes_one_sqlite_row_by_primary_key() {
        let engine = Engine::new();
        let connection_id = engine
            .connect("sqlite://:memory:".to_string(), "delete".to_string())
            .await
            .expect("SQLite connection");
        for sql in [
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)",
            "INSERT INTO users VALUES (1, 'Ada'), (2, 'Grace')",
        ] {
            engine
                .execute_sql(super::QueryRequest {
                    connection_id,
                    database: String::new(),
                    sql: sql.into(),
                    limit: 0,
                })
                .await
                .expect("seed statement");
        }

        let deleted = engine
            .delete_row(DeleteRowRequest {
                connection_id,
                database: String::new(),
                schema: "main".into(),
                table: "users".into(),
                key_column: "id".into(),
                key_value: MutationValue::Number("1".into()),
            })
            .await
            .expect("delete row");
        assert_eq!(deleted, 1);

        let result = engine
            .execute_sql(super::QueryRequest {
                connection_id,
                database: String::new(),
                sql: "SELECT id FROM users".into(),
                limit: 0,
            })
            .await
            .expect("read remaining row");
        assert_eq!(result.total_count, 1);
        engine.disconnect(connection_id).expect("disconnect");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn inserts_sqlite_row_without_explicit_auto_id() {
        let engine = Engine::new();
        let connection_id = engine
            .connect("sqlite://:memory:".to_string(), "insert".to_string())
            .await
            .expect("SQLite connection");
        engine
            .execute_sql(super::QueryRequest {
                connection_id,
                database: String::new(),
                sql: "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL)".into(),
                limit: 0,
            })
            .await
            .expect("create table");

        let inserted = engine
            .insert_row(InsertRowRequest {
                connection_id,
                database: String::new(),
                schema: "main".into(),
                table: "users".into(),
                values: vec![("name".into(), MutationValue::Text("Ada".into()))],
            })
            .await
            .expect("insert row");
        assert_eq!(inserted, 1);

        let result = engine
            .execute_sql(super::QueryRequest {
                connection_id,
                database: String::new(),
                sql: "SELECT id, name FROM users".into(),
                limit: 0,
            })
            .await
            .expect("read inserted row");
        assert_eq!(result.total_count, 1);
        engine.disconnect(connection_id).expect("disconnect");
    }

    #[test]
    fn cloned_engines_share_state_but_new_engines_are_isolated() {
        let first = Engine::new();
        let clone = first.clone();
        let second = Engine::new();

        assert!(Arc::ptr_eq(&first.state, &clone.state));
        assert!(!Arc::ptr_eq(&first.state, &second.state));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn executes_sqlite_query_from_a_non_engine_runtime() {
        let engine = Engine::new();
        let connection_id = engine
            .connect("sqlite://:memory:".to_string(), "test".to_string())
            .await
            .expect("SQLite connection");

        let result = engine
            .execute_sql(super::QueryRequest {
                connection_id,
                database: String::new(),
                sql: "SELECT 42 AS answer".to_string(),
                limit: 0,
            })
            .await
            .expect("SQLite query");

        assert_eq!(result.total_count, 1);
        assert_eq!(result.batches[0].num_columns(), 1);
        assert_eq!(result.batches[0].num_rows(), 1);
        engine.disconnect(connection_id).expect("disconnect");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn executes_sqlite_query_page_with_offset() {
        let engine = Engine::new();
        let connection_id = engine
            .connect("sqlite://:memory:".to_string(), "paging".to_string())
            .await
            .expect("SQLite connection");

        let result = engine
            .execute_sql_page(
                super::QueryRequest {
                    connection_id,
                    database: String::new(),
                    sql: "WITH rows(value) AS (SELECT 0 UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3) SELECT value FROM rows".to_string(),
                    limit: 2,
                },
                2,
            )
            .await
            .expect("SQLite query page");

        assert_eq!(result.total_count, 2);
        let batch = &result.batches[0];
        assert_eq!(batch.num_rows(), 2);
        let values = batch
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .expect("integer result column");
        assert_eq!(values.values(), &[2, 3]);
        engine.disconnect(connection_id).expect("disconnect");
    }

    async fn sqlite_connection_with_users(engine: &Engine) -> super::ConnectionId {
        let connection_id = engine
            .connect("sqlite://:memory:".to_string(), "catalog-test".to_string())
            .await
            .expect("SQLite connection");
        for sql in [
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL)",
            "CREATE VIEW active_users AS SELECT id, name FROM users",
        ] {
            engine
                .execute_sql(super::QueryRequest {
                    connection_id,
                    database: String::new(),
                    sql: sql.to_string(),
                    limit: 0,
                })
                .await
                .expect("seed statement");
        }
        connection_id
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sqlite_catalog_layers_load_without_the_full_catalog() {
        let engine = Engine::new();
        let connection_id = sqlite_connection_with_users(&engine).await;

        let databases = engine
            .list_databases(connection_id)
            .await
            .expect("list databases");
        assert_eq!(databases, vec!["main".to_string()]);

        // SQLite surfaces attached databases where the shell expects schemas.
        let schemas = engine
            .list_schemas(connection_id, String::new())
            .await
            .expect("list schemas");
        assert_eq!(schemas, vec!["main".to_string()]);

        let objects = engine
            .list_objects(connection_id, String::new(), "main".to_string())
            .await
            .expect("list objects");
        assert_eq!(objects.len(), 2);
        assert!(objects.iter().any(|object| {
            object.name == "users" && object.kind == super::CatalogObjectKind::Table
        }));
        assert!(objects.iter().any(|object| {
            object.name == "active_users" && object.kind == super::CatalogObjectKind::View
        }));

        let columns = engine
            .list_columns(
                connection_id,
                String::new(),
                "main".to_string(),
                "users".to_string(),
            )
            .await
            .expect("list columns");
        assert_eq!(columns.len(), 2);
        assert_eq!(columns[0].name, "id");
        assert!(columns[0].primary_key);
        assert_eq!(columns[1].name, "name");
        assert!(!columns[1].nullable);

        engine.disconnect(connection_id).expect("disconnect");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sqlite_filtered_object_listing_matches_names_literally_and_honors_the_limit() {
        let engine = Engine::new();
        let connection_id = sqlite_connection_with_users(&engine).await;
        for statement in [
            "CREATE TABLE user_100 (id INTEGER)",
            "CREATE TABLE userX100 (id INTEGER)",
            "CREATE TABLE \"50%off\" (id INTEGER)",
        ] {
            engine
                .execute_sql(super::QueryRequest {
                    connection_id,
                    database: String::new(),
                    sql: statement.to_string(),
                    limit: 0,
                })
                .await
                .expect("seed statement");
        }
        let names = |objects: Vec<super::CatalogObject>| {
            let mut names: Vec<_> = objects.into_iter().map(|object| object.name).collect();
            names.sort();
            names
        };
        let list = |filter: &str, limit: usize| {
            engine.list_objects_filtered(
                connection_id,
                String::new(),
                "main".to_string(),
                filter.to_string(),
                limit,
            )
        };

        // Case-insensitive substring, tables and views alike.
        assert_eq!(
            names(list("USER", 50).await.unwrap()),
            ["active_users", "userX100", "user_100", "users"]
        );
        // `_` is a literal underscore, not the LIKE single-char wildcard.
        assert_eq!(names(list("user_1", 50).await.unwrap()), ["user_100"]);
        // `%` is literal too; a lone `%` must not match everything.
        assert_eq!(names(list("%", 50).await.unwrap()), ["50%off"]);
        // Quotes and SQL text are inert (bound parameter).
        assert!(list("x'; DROP TABLE users; --", 50).await.unwrap().is_empty());
        assert_eq!(names(list("", 50).await.unwrap()).len(), 5, "empty filter lists everything");
        // The limit is applied server-side.
        assert_eq!(list("user", 2).await.unwrap().len(), 2);
        // The plain listing is unchanged.
        assert_eq!(
            engine
                .list_objects(connection_id, String::new(), "main".to_string())
                .await
                .unwrap()
                .len(),
            5
        );
        engine.disconnect(connection_id).expect("disconnect");
    }

    #[test]
    fn postgres_filtered_objects_sql_binds_filter_and_limit_and_never_interpolates() {
        let sql = crate::db::postgres::POSTGRES_SQL_OBJECTS_FILTERED_SQL;
        // Backslash is PostgreSQL's default LIKE escape, matching escape_like.
        assert!(sql.contains("c.relname ILIKE $2"), "{sql}");
        assert!(!sql.contains("ESCAPE"), "{sql}");
        assert!(sql.contains("LIMIT $3"));
        assert!(sql.contains("n.nspname = $1"));
        // The statement text is constant: user input only appears as parameters.
        assert!(!sql.contains('%') && !sql.contains("{}"));
    }

    #[test]
    fn postgres_local_helpers_match_shared_name_filter_and_arrow_key() {
        use crate::db::postgres::{pg_like_contains_pattern, pg_sql_limit};
        for input in ["", "plain", "50%_off\\x", "a_b", "%%", "\\"] {
            assert_eq!(
                pg_like_contains_pattern(input),
                crate::name_filter::like_contains_pattern(input),
                "{input:?}"
            );
        }
        for limit in [0usize, 1, 500, usize::MAX] {
            assert_eq!(pg_sql_limit(limit), crate::name_filter::sql_limit(limit));
        }
        assert_eq!(
            crate::db::postgres_arrow::PG_NATIVE_TYPE_METADATA_KEY,
            crate::arrow::NATIVE_TYPE_METADATA_KEY
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn catalog_calls_on_unknown_connections_return_typed_errors() {
        let engine = Engine::new();

        let error = engine.list_databases(9999).await.expect_err("unknown id");
        assert!(matches!(
            error,
            super::DbEngineError::Connection(message) if message.contains("9999")
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sqlite_long_query_is_interrupted_by_cancel() {
        let engine = Engine::new();
        let connection_id = sqlite_connection_with_users(&engine).await;

        let query_engine = engine.clone();
        let query_task = tokio::spawn(async move {
            query_engine
                .execute_sql(super::QueryRequest {
                    connection_id,
                    database: String::new(),
                    // Recursive counter that keeps SQLite busy long enough
                    // to prove the interrupt handle works.
                    sql: r#"
                    WITH RECURSIVE cnt(x) AS (
                        SELECT 1
                        UNION ALL
                        SELECT x + 1 FROM cnt WHERE x < 100000000
                    )
                    SELECT count(*) AS total FROM cnt
                    "#
                    .to_string(),
                    limit: 0,
                })
                .await
        });

        // Give the query time to start, then cancel from another task.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        engine.cancel(connection_id, String::new()).await;

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if query_task.is_finished() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "cancelled query did not finish in time"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        let outcome = query_task.await.expect("task join");
        assert!(
            outcome.is_err(),
            "interrupted query must fail, got: {:?}",
            outcome.ok().map(|result| result.total_count)
        );

        // The connection remains usable after the interruption.
        let result = engine
            .execute_sql(super::QueryRequest {
                connection_id,
                database: String::new(),
                sql: "SELECT count(*) AS n FROM users".to_string(),
                limit: 0,
            })
            .await
            .expect("connection still usable after cancel");
        assert_eq!(result.total_count, 1);
        engine.disconnect(connection_id).expect("disconnect");
    }
}

#[cfg(test)]
mod table_properties_tests {
    use super::{postgres_properties_to_catalog, Engine, QueryRequest};
    use crate::catalog::CatalogConstraintKind;
    use crate::db::postgres::{
        SqlColumn, SqlConstraint, SqlForeignKey, SqlIndex, SqlTableProperties, SqlTrigger,
    };

    #[test]
    fn postgres_properties_map_into_catalog_types() {
        let fk = |name: &str| SqlForeignKey {
            name: name.into(),
            table: "public.orders".into(),
            columns: vec!["customer_id".into()],
            referenced_table: "public.customers".into(),
            referenced_columns: vec!["id".into()],
            on_update: "NO ACTION".into(),
            on_delete: "CASCADE".into(),
            definition: "FOREIGN KEY (customer_id) REFERENCES customers(id) ON DELETE CASCADE"
                .into(),
        };
        let mapped = postgres_properties_to_catalog(SqlTableProperties {
            columns: vec![SqlColumn {
                name: "id".into(),
                data_type: "integer".into(),
                nullable: false,
                primary_key: true,
                is_generated: Some(false),
                default_expression: None,
                auto_increment: Some(true),
                comment: None,
            }],
            constraints: ["p", "u", "c", "x", "?"]
                .into_iter()
                .map(|code| SqlConstraint {
                    name: format!("con_{code}"),
                    kind_code: code.into(),
                    columns: vec!["id".into()],
                    definition: "PRIMARY KEY (id)".into(),
                })
                .collect(),
            foreign_keys: vec![fk("own")],
            indexes: vec![SqlIndex {
                name: "orders_pkey".into(),
                columns: vec!["id".into()],
                unique: true,
                primary: true,
                method: "btree".into(),
                definition: "CREATE UNIQUE INDEX ...".into(),
                backs_constraint: true,
            }],
            references: vec![fk("incoming")],
            triggers: vec![SqlTrigger {
                name: "audit".into(),
                timing: "INSTEAD OF".into(),
                events: vec!["INSERT".into(), "UPDATE".into()],
                enabled: false,
                definition: "CREATE TRIGGER audit ...".into(),
            }],
            ddl: "CREATE TABLE ...;".into(),
        });

        assert_eq!(mapped.columns.len(), 1);
        assert!(mapped.columns[0].primary_key);
        let kinds: Vec<_> = mapped.keys.as_ref().expect("keys").iter().map(|k| k.kind).collect();
        assert_eq!(
            kinds,
            [
                CatalogConstraintKind::PrimaryKey,
                CatalogConstraintKind::Unique,
                CatalogConstraintKind::Check,
                CatalogConstraintKind::Exclusion,
                CatalogConstraintKind::Other,
            ]
        );
        let own = &mapped.foreign_keys.as_ref().expect("fks")[0];
        assert_eq!(own.name, "own");
        assert_eq!(own.on_delete.as_deref(), Some("CASCADE"));
        assert_eq!(own.on_update.as_deref(), Some("NO ACTION"));
        assert_eq!(mapped.references.as_ref().expect("refs")[0].name, "incoming");
        let index = &mapped.indexes.as_ref().expect("indexes")[0];
        assert_eq!(index.method.as_deref(), Some("btree"));
        assert!(index.primary && index.unique);
        let trigger = &mapped.triggers.as_ref().expect("triggers")[0];
        assert_eq!(trigger.timing.as_deref(), Some("INSTEAD OF"));
        assert_eq!(trigger.enabled, Some(false));
        assert_eq!(mapped.ddl.as_deref(), Some("CREATE TABLE ...;"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sqlite_table_properties_end_to_end_through_engine() {
        let engine = Engine::new();
        let connection_id = engine
            .connect("sqlite://:memory:".to_string(), "props".to_string())
            .await
            .expect("SQLite connection");
        for sql in [
            "CREATE TABLE customers (id INTEGER PRIMARY KEY, email TEXT NOT NULL UNIQUE)",
            "CREATE TABLE orders (id INTEGER PRIMARY KEY, customer_id INTEGER NOT NULL REFERENCES customers(id) ON DELETE CASCADE, qty INTEGER NOT NULL CHECK (qty > 0), note TEXT)",
            "CREATE INDEX idx_orders_customer ON orders (customer_id)",
            "CREATE UNIQUE INDEX idx_orders_note ON orders (note)",
            "CREATE TRIGGER orders_ai AFTER INSERT ON orders BEGIN SELECT 1; END",
        ] {
            engine
                .execute_sql(QueryRequest {
                    connection_id,
                    database: String::new(),
                    sql: sql.into(),
                    limit: 0,
                })
                .await
                .unwrap_or_else(|error| panic!("{sql}: {error}"));
        }

        let orders = engine
            .table_properties(connection_id, String::new(), "main".into(), "orders".into())
            .await
            .expect("orders properties");
        assert_eq!(orders.columns.len(), 4);
        let keys = orders.keys.expect("keys");
        assert!(keys.iter().any(|k| k.kind == CatalogConstraintKind::PrimaryKey));
        assert!(keys.iter().any(|k| k.kind == CatalogConstraintKind::Check));
        let fks = orders.foreign_keys.expect("fks");
        assert_eq!(fks[0].on_delete.as_deref(), Some("CASCADE"));
        assert_eq!(orders.indexes.expect("indexes").len(), 2);
        assert_eq!(orders.triggers.expect("triggers")[0].name, "orders_ai");
        assert!(orders.references.expect("refs").is_empty());
        let ddl = orders.ddl.expect("ddl");
        assert!(ddl.contains("CREATE TABLE orders") && ddl.contains("CREATE INDEX idx_orders_customer"));

        let customers = engine
            .table_properties(connection_id, String::new(), "main".into(), "customers".into())
            .await
            .expect("customers properties");
        let refs = customers.references.expect("refs");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].table, "orders");
        assert!(customers
            .keys
            .expect("keys")
            .iter()
            .any(|k| k.kind == CatalogConstraintKind::Unique && k.columns == ["email"]));

        assert!(engine
            .table_properties(connection_id, String::new(), "main".into(), "missing".into())
            .await
            .is_err());
        assert!(engine
            .table_properties(9_999, String::new(), "main".into(), "orders".into())
            .await
            .is_err());
        engine.disconnect(connection_id).expect("disconnect");
    }
}
