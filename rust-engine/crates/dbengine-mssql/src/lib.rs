//! SQL Server (and Azure SQL) on the shared driver kit (ABI v2), over TDS.
//!
//! * Transport: `tiberius` (pure Rust TDS) with rustls on the `ring` provider (see
//!   `conn::install_ring`; no OpenSSL). Results are read row by row from the TDS stream into
//!   Arrow builders (`convert`) and handed to the kit, which exports them through the Arrow C
//!   Data Interface; `convert` documents the SQL Server-to-Arrow type mapping.
//! * Connection: `sqlserver://user:password@host[:port][/database]?encrypt=..&trustServerCertificate=..`
//!   (`config`). One TDS session per kit connection plus a lazily opened second one for catalog
//!   calls, so the navigator never waits behind a running statement. Connect, TCP keepalive and
//!   the cancel acknowledgement are all bounded: a dead server is noticed, never waited for.
//! * Cancel: the TDS attention signal. The running statement is dropped, the attention packet is
//!   sent and its acknowledgement drained; the session keeps its state and its transaction
//!   (`conn`). A server that does not acknowledge within ten seconds is declared lost.
//! * Queries (`query`, `sql`): paging by `OFFSET .. FETCH` when the statement has a top-level
//!   `ORDER BY`, else by skipping rows as they arrive; `GO` lines separate batches; T-SQL needs no
//!   `;`, so the words of each statement are scanned for writes the statement splitter cannot see.
//! * Navigator, `describe_object` and mutations: `navigator`, `describe`, `mutate`.
//!
//! Not supported: Windows integrated authentication and Kerberos (SQL Server logins only), named
//! instances (connect by port), `sql_variant` (tiberius cannot read it), and a server-side
//! interrupt of a statement without killing its session (SQL Server has none: `KILL` ends the
//! session).

mod catalog;
mod config;
mod conn;
mod convert;
mod describe;
mod mutate;
mod navigator;
mod query;
mod sql;

use conn::{MsCanceller, MsConnection};
use engine_driver_kit::api::{
    Capability, DriverCapabilities, MutationRequest, NavigatorPage, NavigatorRequest, OperationId,
    QueryRequest, SqlDialectInfo,
};
use engine_driver_kit::{
    BackendError, CellFormatter, ConnectRequest, DriverBackend, MutationOutcome, OperationGuard,
    Page, QueryOutput,
};

const DRIVER: &str = "mssql";

/// The capabilities the driver advertises: exactly what it implements.
fn capabilities() -> DriverCapabilities {
    let mut capabilities = DriverCapabilities::new(
        DRIVER,
        "SQL Server",
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
            Capability::MetadataSchemaColumns,
            Capability::QueryReadOnly,
            Capability::QueryRunToEnd,
        ]
        .map(|capability| capability.as_str().to_owned())
        .into(),
    );
    capabilities.uri_schemes = ["sqlserver", "mssql"].map(String::from).into();
    capabilities.editors = vec!["sql".into()];
    capabilities.navigator_kinds = navigator::NAVIGATOR_KINDS.map(String::from).into();
    capabilities.sql_dialect = Some(SqlDialectInfo {
        // `"` is an identifier quote under QUOTED_IDENTIFIER ON (every client library's
        // default); hosts that know SQL Server quote with `[name]` (`bracket_identifiers`).
        identifier_quote: "\"".into(),
        string_literal_quote: "'".into(),
        supports_schemas: true,
        database_qualified_names: true,
        like_requires_text_cast: false,
        string_backslash_escapes: false,
        double_quote_identifiers: true,
        bracket_identifiers: true,
    });
    capabilities
}

/// SQL Server backend.
struct SqlServer;

impl DriverBackend for SqlServer {
    type Connection = MsConnection;
    type PageContext = ();
    type Canceller = MsCanceller;

    fn capabilities() -> DriverCapabilities {
        capabilities()
    }

    fn connect(request: &ConnectRequest) -> Result<MsConnection, BackendError> {
        MsConnection::connect(&request.uri)
    }

    fn disconnect(connection: &MsConnection) {
        connection.close();
    }

    fn list_nav_nodes(
        connection: &MsConnection,
        request: &NavigatorRequest,
    ) -> Result<NavigatorPage, BackendError> {
        navigator::list_nav_nodes(connection, request)
    }

    fn open_query(
        connection: &MsConnection,
        request: &QueryRequest,
        operation: &mut OperationGuard<'_>,
    ) -> Result<QueryOutput, BackendError> {
        query::open_query(connection, request, operation)
    }

    fn canceller(connection: &MsConnection) -> Result<MsCanceller, BackendError> {
        Ok(connection.canceller())
    }

    /// Wakes the running statement, which sends the attention signal itself; takes no lock.
    fn cancel(canceller: &MsCanceller, operation_id: OperationId) -> Result<(), BackendError> {
        canceller.cancel(operation_id);
        Ok(())
    }

    /// Binary values print as `0x` + upper-case hex, the way SQL Server prints them (the kit's
    /// default is the `X'..'` literal of other dialects).
    fn cell_formatter(page: &Page<()>, column: usize) -> CellFormatter<'_> {
        let array = page.batch.column(column);
        match array.as_any().downcast_ref::<arrow_array::BinaryArray>() {
            Some(values) => CellFormatter::custom(array, move |row, out| {
                const HEX: &[u8; 16] = b"0123456789ABCDEF";
                out.push_str("0x");
                for byte in values.value(row) {
                    out.push(HEX[usize::from(byte >> 4)] as char);
                    out.push(HEX[usize::from(byte & 0x0f)] as char);
                }
            }),
            None => CellFormatter::new(array),
        }
    }

    fn mutate(
        connection: &MsConnection,
        request: &MutationRequest,
        operation: &mut OperationGuard<'_>,
    ) -> Result<MutationOutcome, BackendError> {
        mutate::mutate(connection, request, operation)
    }

    fn describe_object(
        connection: &MsConnection,
        request: &serde_json::Value,
    ) -> Result<serde_json::Value, BackendError> {
        describe::describe_object(connection, request)
    }
}

engine_driver_kit::export_driver!(SqlServer; cancel_operation, mutate, describe_object);

#[cfg(test)]
mod tests {
    use super::*;
    use engine_driver_kit::ExportedSymbols;

    #[test]
    fn capabilities_match_the_exported_symbols_and_describe_the_dialect() {
        let capabilities = capabilities();
        ExportedSymbols::CANCEL_OPERATION
            .union(ExportedSymbols::MUTATE)
            .union(ExportedSymbols::DESCRIBE_OBJECT)
            .validate(&capabilities)
            .expect("exports exactly what is advertised");
        assert_eq!(capabilities.driver, "mssql");
        assert_eq!(capabilities.display_name, "SQL Server");
        assert_eq!(capabilities.uri_schemes, ["sqlserver", "mssql"]);
        assert_eq!(capabilities.editors, ["sql"]);
        for capability in [
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
            Capability::MetadataSchemaColumns,
            Capability::QueryReadOnly,
            Capability::QueryRunToEnd,
        ] {
            assert!(capabilities.has(capability), "{capability}");
        }
        assert!(!capabilities.has(Capability::MutationDropDatabase));
        assert!(!capabilities.has_any_export());
        let dialect = capabilities.sql_dialect.as_ref().expect("dialect");
        assert!(dialect.supports_schemas && dialect.database_qualified_names);
        assert!(dialect.bracket_identifiers && dialect.double_quote_identifiers);
        assert!(!dialect.string_backslash_escapes);
        for kind in ["database", "schema", "table", "view", "column", "procedure"] {
            assert!(capabilities.navigator_kinds.iter().any(|k| k == kind), "{kind}");
        }
    }
}
