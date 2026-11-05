//! PostgreSQL as an ABI v2 driver on `engine-driver-kit` (`abi-v2` feature).
//!
//! The kit owns every `dbengine_*` symbol, the page registry, Arrow leases and
//! the cancel state machine; this module supplies [`PostgresBackend`]. There is
//! no `unsafe` here.
//!
//! # Design
//!
//! * **User session.** [`PgConnection`] wraps the shared `PostgresConn` (the
//!   same code the native engine uses: default client plus a lazily opened client
//!   per database; a database name is set on the parsed `Config`, never pasted
//!   into a URI) behind a gate that serializes queries and mutations. One
//!   statement at a time means a `CancelToken` always reaches the statement the
//!   host asked to cancel, and a user's own `BEGIN ... COMMIT` spans calls.
//! * **Metadata connection.** Navigator and describe run on a second connection
//!   opened on first use, so they never wait behind a running user query; they
//!   are bounded by a timeout and a failed metadata connection is reopened.
//!   Costs to know: it has its own per-database client pool, so a kit connection
//!   can hold up to two clients per database it touches (user session + metadata)
//!   and the server sees twice the connections; and it is a different session, so
//!   it cannot see the user session's *uncommitted* DDL (a table created inside an
//!   open `BEGIN` appears in the navigator and in `describe` only after `COMMIT`).
//! * **Cancel without the gate, by operation id.** The running statement's token
//!   is armed in [`connection::CancelState`] together with its operation id;
//!   `cancel` delivers a request only for that id and never takes the gate, and
//!   finishing an operation waits for cancels still in flight so one can never
//!   hit the next statement. A requested cancel is `query.cancelled` /
//!   `mutation.cancelled`; an interrupt nobody asked for is a retryable
//!   `*.interrupted`.
//! * **Broken connections.** A failed `ROLLBACK` or a gate poisoned by a panic
//!   marks the connection broken: later calls fail with `<area>.not_connected`.
//! * **SQL classification** comes from the kit's classifier (`query`): paging,
//!   confirmation of dangerous statements, scripts.
//! * **Arrow end to end.** See [`convert`]: rows stream from the wire into Arrow
//!   builders; `numeric` is rendered from its wire digits straight into a UTF-8
//!   column.
//! * **One runtime** ([`runtime`]) that survives `shutdown` -> `init`.
//!
//! * **TLS** ([`tls`]): libpq `sslmode` semantics on rustls for the user session,
//!   the metadata and per-database connections and the cancel side channel;
//!   `sslrootcert`, `sslcert`/`sslkey` and `hostaddr` are honoured and failures
//!   carry `connect.tls_*` codes. The connection's [`tls::TlsInfo`] (version,
//!   cipher, how far the server was verified) is available inside the driver; the
//!   `connect` response has no field for it yet (`Connected` carries only the
//!   connection id), so it is not on the wire.
//!
//! Wire shapes are documented where they are implemented: [`navigator`],
//! [`query`], [`mutation`], [`describe`].

mod connection;
mod convert;
mod describe;
mod mutation;
mod navigator;
mod query;
mod runtime;
mod schema_objects;
pub(crate) mod tls;

use crate::db::postgres::PostgresConn;
use arrow_array::RecordBatch;
use arrow_schema::DataType;
use connection::{PgConnection, CATALOG_TIMEOUT};
use engine_driver_kit::api::{
    Capability, DriverCapabilities, MutationRequest, NavigatorPage, NavigatorRequest, OperationId,
    PageColumn, QueryRequest, SqlDialectInfo,
};
use engine_driver_kit::{
    BackendError, ConnectRequest, DriverBackend, MutationOutcome, OperationGuard, QueryOutput,
};
use std::sync::Arc;

const DRIVER: &str = "postgresql";

/// Page-column descriptors for `batch` (`int64`, `float32`, `float64`, `boolean`, `binary`, `text`).
pub(crate) fn page_columns(batch: &RecordBatch) -> Vec<PageColumn> {
    engine_driver_kit::page_columns(batch, |data_type| {
        match data_type {
            DataType::Int64 => "int64",
            DataType::Float32 => "float32",
            DataType::Float64 => "float64",
            DataType::Boolean => "boolean",
            // Text that is not UTF-8 (the native type says `<type> (invalid utf-8)`).
            DataType::Binary => "binary",
            _ => "text",
        }
        .to_owned()
    })
}

pub(crate) struct PostgresBackend;

impl DriverBackend for PostgresBackend {
    type Connection = PgConnection;
    type PageContext = ();
    type Canceller = Arc<connection::CancelState>;

    fn capabilities() -> DriverCapabilities {
        let mut capabilities = DriverCapabilities::new(
            DRIVER,
            "PostgreSQL",
            [
                Capability::Navigator,
                Capability::QuerySql,
                Capability::QueryArrowCData,
                Capability::OperationCancel,
                Capability::MutationInsert,
                Capability::MutationUpdate,
                Capability::MutationDelete,
                Capability::MutationBatch,
                Capability::DatabaseCreate,
                Capability::MetadataTableProperties,
                Capability::QueryReadOnly,
                Capability::QueryRunToEnd,
            ]
            .map(|capability| capability.as_str().to_owned())
            .into(),
        );
        capabilities.uri_schemes = vec!["postgres".into(), "postgresql".into()];
        capabilities.editors = vec!["sql".into()];
        capabilities.navigator_kinds = navigator::NAVIGATOR_KINDS.map(String::from).into();
        capabilities.sql_dialect = Some(SqlDialectInfo {
            identifier_quote: "\"".into(),
            string_literal_quote: "'".into(),
            supports_schemas: true,
            database_qualified_names: false,
            like_requires_text_cast: true,
            // `standard_conforming_strings` is on: `'\'` is one backslash.
            string_backslash_escapes: false,
            double_quote_identifiers: true,
            bracket_identifiers: false,
        });
        capabilities
    }

    fn init() -> Result<(), BackendError> {
        runtime::handle().map(drop).map_err(BackendError::new)
    }

    /// Every connection is already gone; stop the runtime so no worker thread
    /// outlives a `dlclose`. The next `init` (or call) builds a new one.
    fn shutdown() {
        runtime::stop();
    }

    fn connect(request: &ConnectRequest) -> Result<PgConnection, BackendError> {
        crate::validate_postgres_uri(&request.uri)
            .map_err(|message| BackendError::with_code("connect.bad_request", message))?;
        let uri = request.uri.as_str();
        // Typed errors (`connect.tls_*`) come from `v2_connect`, so the timeout
        // wrapper must not flatten them into a message.
        let inner = runtime::block_on(async {
            tokio::time::timeout(CATALOG_TIMEOUT, PostgresConn::v2_connect(uri)).await
        })
        .map_err(BackendError::new)?
        .map_err(|_| BackendError::new("PostgreSQL connection timed out"))??;
        Ok(PgConnection::new(inner))
    }

    /// `server_version_num` in the numeric form MySQL reports, so the overview shows `16.2.0`.
    /// A server that does not answer in time reports none; the connection still works.
    fn connection_server_version(connection: &PgConnection) -> Option<u32> {
        runtime::block_on(async {
            tokio::time::timeout(
                CATALOG_TIMEOUT,
                schema_objects::host_server_version(&connection.inner),
            )
            .await
        })
        .ok()?
        .ok()?
    }

    fn list_nav_nodes(
        connection: &PgConnection,
        request: &NavigatorRequest,
    ) -> Result<NavigatorPage, BackendError> {
        navigator::list(connection, request)
    }

    fn open_query(
        connection: &PgConnection,
        request: &QueryRequest,
        operation: &mut OperationGuard<'_>,
    ) -> Result<QueryOutput, BackendError> {
        query::open_query(connection, request, operation)
    }

    fn canceller(connection: &PgConnection) -> Result<Arc<connection::CancelState>, BackendError> {
        Ok(Arc::clone(&connection.cancel))
    }

    fn cancel(
        canceller: &Arc<connection::CancelState>,
        operation_id: OperationId,
    ) -> Result<(), BackendError> {
        canceller.cancel(operation_id)
    }

    fn mutate(
        connection: &PgConnection,
        request: &MutationRequest,
        operation: &mut OperationGuard<'_>,
    ) -> Result<MutationOutcome, BackendError> {
        mutation::mutate(connection, request, operation)
    }

    fn describe_object(
        connection: &PgConnection,
        request: &serde_json::Value,
    ) -> Result<serde_json::Value, BackendError> {
        describe::describe_object(connection, request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_driver_kit::ExportedSymbols;

    #[test]
    fn exported_symbols_match_capabilities() {
        ExportedSymbols::CANCEL_OPERATION
            .union(ExportedSymbols::MUTATE)
            .union(ExportedSymbols::DESCRIBE_OBJECT)
            .validate(&PostgresBackend::capabilities())
            .expect("PostgreSQL exports exactly the optional symbols it advertises");
    }

    #[test]
    fn capabilities_document_is_complete_and_round_trips() {
        let capabilities = PostgresBackend::capabilities();
        let json = serde_json::to_string(&capabilities).unwrap();
        let parsed: DriverCapabilities = serde_json::from_str(&json).unwrap();
        for capability in [
            Capability::Navigator,
            Capability::QuerySql,
            Capability::QueryRunToEnd,
            Capability::QueryArrowCData,
            Capability::OperationCancel,
            Capability::MutationInsert,
            Capability::MutationUpdate,
            Capability::MutationDelete,
            Capability::MutationBatch,
            Capability::DatabaseCreate,
            Capability::MetadataTableProperties,
            Capability::QueryReadOnly,
        ] {
            assert!(parsed.has(capability), "{capability}");
        }
        assert!(!parsed.has(Capability::QueryPageLease));
        assert!(!parsed.has_any_export());
        assert!(parsed.supports_mutation_kind("create_database"));
        assert!(parsed.supports_mutation_kind("update"));
        assert!(parsed.supports_mutation_kind("batch"));
        assert_eq!(parsed.editors, ["sql"]);
        assert_eq!(parsed.uri_schemes, ["postgres", "postgresql"]);
        let dialect = parsed.sql_dialect.expect("dialect");
        assert!(dialect.supports_schemas && dialect.like_requires_text_cast);
        assert!(!dialect.database_qualified_names);
        assert!(!dialect.string_backslash_escapes);
    }

    #[test]
    fn non_postgres_uris_are_rejected_before_any_network_use() {
        let request: ConnectRequest =
            serde_json::from_str(r#"{"uri":"mongodb://localhost:27017"}"#).unwrap();
        let error = PostgresBackend::connect(&request).err().expect("rejected");
        assert_eq!(error.code.as_deref(), Some("connect.bad_request"));
    }
}
