//! Reading the roles, memberships and privileges of a connection, on a session of its own and off the UI thread.
//!
//! A load opens a second session of the connection (the one a structure apply uses: its own profile, credentials and tunnel),
//! runs the catalog queries of `sql_account::catalog` on it, and closes it. The window's own session is not used, so a
//! transaction open in a SQL tab is neither joined nor disturbed.

use engine_driver_api::QueryRequest;
use engine_driver_host::DriverSession;

use super::super::structure_edit::apply::SessionSpec;
use crate::connections::ConnectionKind;
use crate::grid_arrow::{ArrowCellReader, BinaryDisplay};
use crate::sql_account::catalog::{self, AccountCatalog, CatalogSession, PrivilegeCatalog, ReadError, TextRows};
use crate::sql_account::{AccountEngine, AccountId};
use crate::sql_ddl::dialect_in_mode;

/// The most rows one catalog query returns (the roles of a server, the tables the editor lists).
const ROW_LIMIT: u64 = 50_000;

/// What to read.
pub(in crate::app) enum LoadJob {
    Accounts { engine: AccountEngine },
    Privileges { kind: ConnectionKind, sql_mode: String, id: AccountId, database: Option<String> },
}

/// What was read.
pub(in crate::app) enum Loaded {
    Accounts(Box<AccountCatalog>),
    Privileges(Box<PrivilegeCatalog>),
}

/// Where the queries go.
pub(in crate::app) enum CatalogSource {
    Driver(Box<SessionSpec>),
    #[cfg(test)]
    Injected(std::sync::Arc<InjectedCatalog>),
}

/// Test double: the session a load reads from, made for the database it is asked for.
#[cfg(test)]
pub(in crate::app) type InjectedCatalog = dyn Fn(Option<&str>) -> Box<dyn CatalogSession + Send> + Send + Sync;

impl CatalogSource {
    /// Runs `job`, blocking: call it off the UI thread.
    pub(in crate::app) fn run(self, job: LoadJob) -> Result<Loaded, ReadError> {
        let database = match &job {
            LoadJob::Privileges { database, .. } => database.clone(),
            LoadJob::Accounts { .. } => None,
        };
        let mut session: Box<dyn CatalogSession> = match self {
            Self::Driver(spec) => {
                let (session, tunnel) = spec.open().map_err(ReadError::Failed)?;
                Box::new(DriverCatalog { session, database, _tunnel: tunnel })
            }
            #[cfg(test)]
            Self::Injected(make) => {
                let session = make(database.as_deref());
                let session: Box<dyn CatalogSession> = session;
                session
            }
        };
        match job {
            LoadJob::Accounts { engine } => catalog::load_accounts(&mut *session, engine).map(|catalog| Loaded::Accounts(Box::new(catalog))),
            LoadJob::Privileges { kind, sql_mode, id, .. } => {
                let engine = AccountEngine::of(kind).ok_or_else(|| ReadError::Failed("This connection has no users and privileges to read.".to_owned()))?;
                let dialect = dialect_in_mode(kind, &sql_mode).ok_or_else(|| ReadError::Failed("This connection has no SQL dialect.".to_owned()))?;
                catalog::load_privileges(&mut *session, &*dialect, engine, &id).map(|catalog| Loaded::Privileges(Box::new(catalog)))
            }
        }
    }
}

/// A driver session that answers catalog queries.
pub(in crate::app) struct DriverCatalog {
    session: DriverSession,
    database: Option<String>,
    /// Declared last: the tunnel goes after the session has disconnected through it.
    _tunnel: Option<engine_connect::connect::LiveTunnel>,
}

#[cfg(test)]
impl DriverCatalog {
    /// A session of the tests' own (another user of a live server) as a catalog session.
    pub(in crate::app) fn of_session(session: DriverSession) -> Self {
        Self { session, database: None, _tunnel: None }
    }
}

fn failure(error: &engine_driver_host::LoaderError) -> ReadError {
    let text = crate::sessions::redact_secrets(&error.to_string());
    ReadError::from_server(super::super::data_import::message_of(&text))
}

impl DriverCatalog {
    /// Opens the session `spec` describes (blocking: call it off the UI thread).
    pub(in crate::app) fn open(spec: SessionSpec) -> Result<Self, ReadError> {
        let (session, tunnel) = spec.open().map_err(ReadError::Failed)?;
        Ok(Self { session, database: None, _tunnel: tunnel })
    }

    fn send(&mut self, sql: &str) -> Result<engine_driver_api::QueryPage, ReadError> {
        let operation = self.session.begin_operation().map_err(|error| failure(&error))?;
        let request = QueryRequest {
            kind: "sql".into(),
            operation_id: operation.id(),
            database: self.database.clone(),
            object: None,
            offset: 0,
            limit: ROW_LIMIT,
            payload: serde_json::json!({ "sql": sql, "confirmed": true }),
        };
        let json = serde_json::to_string(&request).map_err(|error| ReadError::Malformed(error.to_string()))?;
        self.session.open_query(&json).map_err(|error| failure(&error))
    }

    /// Runs one statement and drops what it answers (a `KILL`, a `pg_cancel_backend`).
    pub(in crate::app) fn run(&mut self, sql: &str) -> Result<(), ReadError> {
        let page = self.send(sql)?;
        let _ = self.session.release_page(page.page_id);
        Ok(())
    }
}

impl CatalogSession for DriverCatalog {
    fn rows(&mut self, sql: &str) -> Result<TextRows, ReadError> {
        let page = self.send(sql)?;
        let read = (|| {
            if page.flags.truncated {
                return Err(ReadError::Failed(format!("The server has more than {ROW_LIMIT} rows here.")));
            }
            let lease = self
                .session
                .acquire_arrow_page(page.page_id, 0, page.row_count)
                .map_err(|error| ReadError::Malformed(error.to_string()))?;
            // SAFETY: the lease holds the Arrow C Data pair the driver exported for this page; import consumes it once.
            let imported = unsafe { lease.into_record_batch() }.map_err(|error| ReadError::Malformed(error.to_string()))?;
            let batch = &imported.batch;
            let readers: Vec<ArrowCellReader<'_>> = batch.columns().iter().map(|column| ArrowCellReader::new(column.as_ref(), BinaryDisplay::Hex)).collect();
            let mut buffer = String::new();
            Ok((0..batch.num_rows())
                .map(|row| readers.iter().map(|reader| (!reader.is_null(row)).then(|| reader.text(row, &mut buffer).to_owned())).collect())
                .collect())
        })();
        let _ = self.session.release_page(page.page_id);
        read
    }
}

impl Drop for DriverCatalog {
    fn drop(&mut self) {
        let _ = self.session.disconnect();
    }
}
