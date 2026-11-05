//! ABI v2 driver for the MySQL protocol family, shared by `dbengine-mysql` and
//! `dbengine-mariadb`.
//!
//! One implementation, two flavors. Each cdylib supplies a [`Flavor`] (driver
//! id, display name, URI schemes and the [`Dialect`] for the few places where
//! the servers really differ) and exports `Backend<Flavor>` through
//! `engine_driver_kit::export_driver!`. `dbengine-mariadb` compiles this module
//! from `dbengine-mysql/src/family` via `#[path]`, the same way both crates
//! already shared `dbengine/src/db/mysql.rs`.
//!
//! The kit owns the FFI; this module has no `unsafe`. A kit connection is one
//! `mysql_async::Conn` behind a `Mutex`, driven on a small shared Tokio runtime,
//! plus a lazily opened second connection for catalog calls (navigator,
//! describe) so they never wait behind a running user statement.
//!
//! Session settings worth knowing:
//!
//! * `CLIENT_FOUND_ROWS` is **on** (`client_found_rows(true)`): the affected-row
//!   count of an `UPDATE` is the number of rows it *matched*, not the number it
//!   changed. A keyed mutation that sets a column to the value it already has
//!   therefore reports one row instead of being mistaken for "no such row". This
//!   also applies to `UPDATE`s run through `open_query`.
//! * `sql_mode` is read at connect (and again after user SQL that mentions it):
//!   `NO_BACKSLASH_ESCAPES` and `ANSI_QUOTES` change how SQL text is lexed for
//!   `engine_driver_kit::sql::classify`.
//!   **Limitation:** a request is classified and scanned with the `sql_mode` in
//!   force when it starts. A script that changes `sql_mode` and relies on the new
//!   mode for a later statement of the same script is still read with the old
//!   one (the next request sees the new mode). The classifier is a lexer, so
//!   statements hidden in `PREPARE ... FROM '...'`, stored routines or functions
//!   with side effects are not seen either.
//! * The transaction state is the server's: the status flags of its last answer,
//!   refreshed with a no-op `DO 0` after an error (never guessed from the text of
//!   the statements).
//! * A re-page (`offset > 0`) of a pure read outside a user transaction runs in
//!   `START TRANSACTION READ ONLY` .. `ROLLBACK`, so a write hidden in the read
//!   (a stored function, MariaDB `NEXTVAL`) fails with error 1792 instead of
//!   repeating. It does not stop side effects that are not table writes
//!   (`GET_LOCK`, `SLEEP`). Inside a transaction the *user* opened the statement
//!   just runs there (their transaction decides), and the first page (offset 0)
//!   always runs exactly as written.
//! * A script returns the rows of its last statement that produces a result set;
//!   only a single pure read is paged.
//! * The default database is tracked; any user SQL that may change it (`USE`,
//!   also behind comments or in `/*! */`, and `DROP`) makes the driver re-select
//!   the requested database on the next request.
//!
//! The wire rules every SQL driver follows (paging, confirmation, mutations,
//! savepoints, error codes) are in `engine_driver_api::sql`.

mod conn;
mod describe;
mod mutate;
mod navigator;
mod query;
mod schema_objects;
mod sql;
mod tls;

use conn::{MyCanceller, MyConnection};
use engine_driver_kit::api::{
    Capability, DriverCapabilities, MutationRequest, NavigatorPage, NavigatorRequest, OperationId,
    QueryRequest, SqlDialectInfo,
};
use engine_driver_kit::{
    BackendError, CellFormatter, ConnectRequest, DriverBackend, MutationOutcome, OperationGuard,
    Page, QueryOutput,
};
use std::marker::PhantomData;

/// Where MySQL and MariaDB differ in ways the driver has to care about.
///
/// This module is compiled into both cdylibs and each constructs only its own
/// variant, hence the `dead_code` allowance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Dialect {
    MySql,
    MariaDb,
}

/// Static description of one server flavor.
pub struct FlavorConfig {
    pub driver: &'static str,
    pub display_name: &'static str,
    pub uri_schemes: &'static [&'static str],
    pub dialect: Dialect,
}

/// A server flavor. Implemented by a zero-sized marker in each cdylib.
pub trait Flavor: 'static {
    const CONFIG: FlavorConfig;
}

/// The shared driver backend for flavor `F`.
pub struct Backend<F: Flavor>(PhantomData<fn() -> F>);

/// Capabilities of flavor `F`.
pub fn capabilities<F: Flavor>() -> DriverCapabilities {
    let config = F::CONFIG;
    let mut capabilities = DriverCapabilities::new(
        config.driver,
        config.display_name,
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
    capabilities.uri_schemes = config
        .uri_schemes
        .iter()
        .map(|scheme| (*scheme).to_owned())
        .collect();
    capabilities.editors = vec!["sql".into()];
    capabilities.navigator_kinds = ["database", "table", "view", "function", "procedure", "trigger", "event", "column"]
        .into_iter()
        .chain((config.dialect == Dialect::MariaDb).then_some("sequence"))
        .map(String::from)
        .collect();
    capabilities.sql_dialect = Some(SqlDialectInfo {
        identifier_quote: "`".into(),
        string_literal_quote: "'".into(),
        supports_schemas: false,
        database_qualified_names: true,
        // MySQL/MariaDB coerce any operand for `LIKE`.
        like_requires_text_cast: false,
        // The server default. Capabilities are static per driver; a session
        // running `NO_BACKSLASH_ESCAPES` says so in the `sql_dialect` field of its
        // `dbengine_connect` reply (`DriverBackend::connection_dialect`), as does
        // one with `ANSI_QUOTES` or (MariaDB) `MSSQL`.
        string_backslash_escapes: true,
        double_quote_identifiers: false,
        bracket_identifiers: false,
    });
    capabilities
}

impl<F: Flavor> DriverBackend for Backend<F> {
    type Connection = MyConnection;
    type PageContext = ();
    type Canceller = MyCanceller;

    fn capabilities() -> DriverCapabilities {
        capabilities::<F>()
    }

    fn connect(request: &ConnectRequest) -> Result<MyConnection, BackendError> {
        MyConnection::connect::<F>(&request.uri)
    }

    /// The advertised dialect with the session's real `sql_mode` state
    /// (the capabilities are static and say the server default: backslash
    /// escapes, `"` a string, no `[name]`).
    fn connection_dialect(connection: &MyConnection) -> Option<SqlDialectInfo> {
        let mut dialect = capabilities::<F>().sql_dialect?;
        if let Some(lex) = connection.lex() {
            dialect.string_backslash_escapes = lex.backslash_escapes;
            dialect.double_quote_identifiers = lex.double_quote_is_identifier;
            dialect.bracket_identifiers = lex.bracket_identifiers;
        }
        Some(dialect)
    }

    fn connection_server_version(connection: &MyConnection) -> Option<u32> {
        connection.server_version()
    }

    fn disconnect(connection: &MyConnection) {
        connection.close();
    }

    fn list_nav_nodes(
        connection: &MyConnection,
        request: &NavigatorRequest,
    ) -> Result<NavigatorPage, BackendError> {
        navigator::list_nav_nodes(connection, F::CONFIG.dialect, request)
    }

    fn open_query(
        connection: &MyConnection,
        request: &QueryRequest,
        operation: &mut OperationGuard<'_>,
    ) -> Result<QueryOutput, BackendError> {
        query::open_query(connection, request, operation)
    }

    fn canceller(connection: &MyConnection) -> Result<MyCanceller, BackendError> {
        Ok(connection.canceller())
    }

    /// `KILL QUERY` over a second connection; takes no connection lock.
    fn cancel(canceller: &MyCanceller, operation_id: OperationId) -> Result<(), BackendError> {
        canceller.kill_query(operation_id)
    }

    /// Unsigned BIGINT is the one Arrow type the kit's fast path does not print
    /// directly; format it without Arrow's generic display machinery.
    fn cell_formatter(page: &Page<()>, column: usize) -> CellFormatter<'_> {
        let array = page.batch.column(column);
        match array.as_any().downcast_ref::<arrow_array::UInt64Array>() {
            Some(values) => CellFormatter::custom(array, move |row, out| {
                use std::fmt::Write;
                let _ = write!(out, "{}", values.value(row));
            }),
            None => CellFormatter::new(array),
        }
    }

    fn mutate(
        connection: &MyConnection,
        request: &MutationRequest,
        operation: &mut OperationGuard<'_>,
    ) -> Result<MutationOutcome, BackendError> {
        mutate::mutate(connection, request, operation, &capabilities::<F>())
    }

    fn describe_object(
        connection: &MyConnection,
        request: &serde_json::Value,
    ) -> Result<serde_json::Value, BackendError> {
        describe::describe_object(connection, F::CONFIG.dialect, request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_driver_kit::ExportedSymbols;

    struct Probe;
    impl Flavor for Probe {
        const CONFIG: FlavorConfig = FlavorConfig {
            driver: "probe",
            display_name: "Probe",
            uri_schemes: &["probe"],
            dialect: Dialect::MySql,
        };
    }

    #[test]
    fn capabilities_are_typed_and_match_the_exported_symbols() {
        let capabilities = capabilities::<Probe>();
        ExportedSymbols::CANCEL_OPERATION
            .union(ExportedSymbols::MUTATE)
            .union(ExportedSymbols::DESCRIBE_OBJECT)
            .validate(&capabilities)
            .expect("exports exactly what is advertised");
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
            Capability::QueryReadOnly,
        ] {
            assert!(capabilities.has(capability), "{capability}");
        }
        assert!(!capabilities.has(Capability::MutationDropDatabase));
        assert_eq!(capabilities.editors, ["sql"]);
        assert_eq!(
            capabilities.navigator_kinds,
            ["database", "table", "view", "function", "procedure", "trigger", "event", "column"]
        );
        let dialect = capabilities.sql_dialect.as_ref().expect("dialect");
        assert_eq!(dialect.identifier_quote, "`");
        assert_eq!(dialect.string_literal_quote, "'");
        assert!(!dialect.supports_schemas);
        assert!(dialect.database_qualified_names);
        assert!(!dialect.like_requires_text_cast);
        assert!(dialect.string_backslash_escapes);
        assert!(capabilities.supports_mutation_kind("create_database"));
        assert!(capabilities.supports_mutation_kind("insert"));
        assert!(capabilities.supports_mutation_kind("batch"));
    }
}
