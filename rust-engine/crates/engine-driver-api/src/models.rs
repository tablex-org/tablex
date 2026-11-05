use crate::capability::{Capability, CapabilitySet};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuccessEnvelope<T> {
    pub ok: bool,
    pub value: T,
}

/// SQL syntax a driver speaks, for SQL-capable drivers (`query.sql`). Optional
/// on the wire: hosts fall back to per-kind defaults when it is absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SqlDialectInfo {
    /// Identifier quote (`"` or `` ` ``); an embedded quote is escaped by doubling.
    pub identifier_quote: String,
    /// String-literal quote (`'`); an embedded quote is escaped by doubling.
    pub string_literal_quote: String,
    /// The navigator exposes a schema level between database and object, so
    /// qualified names are `schema.object`.
    pub supports_schemas: bool,
    /// Qualified names may carry a database/catalog prefix (`db.object`).
    pub database_qualified_names: bool,
    /// `LIKE`/`ILIKE` only accepts text operands, so a filter over a column of
    /// any other type must cast it first (`col::text LIKE ...`, PostgreSQL).
    /// Absent on the wire (older drivers) means `false`.
    #[serde(default)]
    pub like_requires_text_cast: bool,
    /// A backslash inside a string literal is an escape character, so a
    /// literal built by the host must double it (`'a\\b'`). MySQL and MariaDB
    /// unless the session runs with `NO_BACKSLASH_ESCAPES`; PostgreSQL (with
    /// `standard_conforming_strings`) and SQLite never. Absent on the wire
    /// (older drivers) means `false`.
    #[serde(default)]
    pub string_backslash_escapes: bool,
    /// `"…"` quotes an identifier (`""` for an embedded quote) rather than a
    /// string. PostgreSQL, SQLite and ClickHouse always; MySQL and MariaDB only
    /// when the session's `sql_mode` has `ANSI_QUOTES` (or `ANSI`, or MariaDB's
    /// `MSSQL`). Absent on the wire (older drivers) means `false`.
    ///
    /// `false` does not say `"` is a string: hosts only add this quoting on top of
    /// their per-engine preset (`SqlLexOptions::with_session_dialect`).
    #[serde(default)]
    pub double_quote_identifiers: bool,
    /// `[name]` quotes an identifier (`]]` for an embedded `]`). SQLite, and
    /// MariaDB under `sql_mode=MSSQL`. Absent on the wire (older drivers)
    /// means `false`.
    ///
    /// `false` does not say `[` is not a name (SQLite always has it): hosts only add
    /// this quoting on top of their per-engine preset.
    #[serde(default)]
    pub bracket_identifiers: bool,
}

/// Driver capabilities as returned by `dbengine_capabilities`.
///
/// `features` keeps the v2 wire shape (a list of tokens). Membership checks go
/// through the [`CapabilitySet`] built once at deserialization, so [`has`]
/// never scans strings. Unknown tokens stay in `features` and round-trip.
///
/// [`has`]: DriverCapabilities::has
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "DriverCapabilitiesWire")]
pub struct DriverCapabilities {
    pub abi_version: u32,
    pub driver: String,
    pub display_name: String,
    pub uri_schemes: Vec<String>,
    features: Vec<String>,
    #[serde(skip)]
    set: CapabilitySet,
    pub editors: Vec<String>,
    pub navigator_kinds: Vec<String>,
    pub single_connection_pool: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sql_dialect: Option<SqlDialectInfo>,
}

/// Deserialization shape of [`DriverCapabilities`] (the wire contract).
#[derive(Deserialize)]
struct DriverCapabilitiesWire {
    abi_version: u32,
    driver: String,
    display_name: String,
    uri_schemes: Vec<String>,
    features: Vec<String>,
    editors: Vec<String>,
    navigator_kinds: Vec<String>,
    single_connection_pool: bool,
    #[serde(default)]
    sql_dialect: Option<SqlDialectInfo>,
}

impl From<DriverCapabilitiesWire> for DriverCapabilities {
    fn from(wire: DriverCapabilitiesWire) -> Self {
        let set = CapabilitySet::from_tokens(wire.features.iter().map(String::as_str));
        Self {
            abi_version: wire.abi_version,
            driver: wire.driver,
            display_name: wire.display_name,
            uri_schemes: wire.uri_schemes,
            features: wire.features,
            set,
            editors: wire.editors,
            navigator_kinds: wire.navigator_kinds,
            single_connection_pool: wire.single_connection_pool,
            sql_dialect: wire.sql_dialect,
        }
    }
}

impl DriverCapabilities {
    /// Capabilities for the current [`ABI_VERSION`](crate::ABI_VERSION) with no
    /// editors, navigator kinds or URI schemes; set those public fields as needed.
    pub fn new(
        driver: impl Into<String>,
        display_name: impl Into<String>,
        features: Vec<String>,
    ) -> Self {
        let set = CapabilitySet::from_tokens(features.iter().map(String::as_str));
        Self {
            abi_version: crate::ABI_VERSION,
            driver: driver.into(),
            display_name: display_name.into(),
            uri_schemes: Vec::new(),
            features,
            set,
            editors: Vec::new(),
            navigator_kinds: Vec::new(),
            single_connection_pool: true,
            sql_dialect: None,
        }
    }

    /// Advertised tokens exactly as sent on the wire (including unknown ones).
    pub fn features(&self) -> &[String] {
        &self.features
    }

    /// O(1): whether the driver advertises `capability`.
    #[inline]
    pub fn has(&self, capability: Capability) -> bool {
        self.set.contains(capability)
    }

    /// Any `mutation.*` token (gates the optional `dbengine_mutate` symbol).
    #[inline]
    pub fn has_any_mutation(&self) -> bool {
        self.set.has_any_mutation()
    }

    /// Any `export.*` token (gates the optional `dbengine_export` symbol).
    #[inline]
    pub fn has_any_export(&self) -> bool {
        self.set.has_any_export()
    }

    /// Exact-token check for tokens that may be outside [`Capability`]. Known
    /// tokens resolve through the set; unknown ones scan `features` (cold path).
    pub fn has_token(&self, token: &str) -> bool {
        match Capability::parse(token) {
            Some(capability) => self.has(capability),
            None => self.features.iter().any(|candidate| candidate == token),
        }
    }

    /// Whether a `MutationRequest` of `kind` is advertised. Kinds without a
    /// [`Capability`] fall back to an exact token scan.
    pub fn supports_mutation_kind(&self, kind: &str) -> bool {
        match Capability::for_mutation_kind(kind) {
            Some(capability) => self.has(capability),
            None => self.features.iter().any(|candidate| {
                candidate
                    .strip_prefix("mutation.")
                    .is_some_and(|suffix| suffix == kind)
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NavigatorNode {
    pub id: String,
    pub kind: String,
    pub label: String,
    pub subtitle: Option<String>,
    pub icon: Option<String>,
    pub has_children: bool,
    pub payload: serde_json::Value,
    pub actions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NavigatorPage {
    pub nodes: Vec<NavigatorNode>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NavigatorRequest {
    pub parent: Option<String>,
    pub database: Option<String>,
    pub payload: Option<serde_json::Value>,
    pub cursor: Option<String>,
    pub limit: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OperationId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageColumn {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub data_type: String,
    pub nullable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageFlags {
    pub truncated: bool,
    pub cancelled: bool,
    pub read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryPage {
    pub operation_id: OperationId,
    pub page_id: u64,
    pub columns: Vec<PageColumn>,
    pub row_count: u64,
    pub total_count: Option<u64>,
    pub total_is_exact: bool,
    pub flags: PageFlags,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryRequest {
    pub kind: String,
    pub operation_id: OperationId,
    pub database: Option<String>,
    pub object: Option<String>,
    pub offset: u64,
    pub limit: u64,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RowField {
    pub name: String,
    pub native_type: String,
    pub display_text: String,
    pub value: serde_json::Value,
    pub editable: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RowFields {
    pub page_id: u64,
    pub row_index: u64,
    pub fields: Vec<RowField>,
    pub mutation_identity: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MutationRequest {
    pub operation_id: OperationId,
    pub kind: String,
    pub database: Option<String>,
    pub object: Option<String>,
    pub identity: serde_json::Value,
    pub payload: serde_json::Value,
}

#[cfg(test)]
mod create_database_tests {
    use super::*;

    #[test]
    fn create_database_request_keeps_v2_shape_and_old_mutations_round_trip() {
        let request: MutationRequest = serde_json::from_str(r#"{"operation_id":5,"kind":"create_database","database":"reports","object":null,"identity":null,"payload":null}"#).unwrap();
        assert_eq!(request.kind, "create_database");
        assert_eq!(request.database.as_deref(), Some("reports"));
        assert_eq!(serde_json::to_value(&request).unwrap()["operation_id"], 5);
        let old: MutationRequest = serde_json::from_str(r#"{"operation_id":4,"kind":"insert","database":"app","object":"items","identity":{},"payload":{"x":1}}"#).unwrap();
        assert_eq!(old.object.as_deref(), Some("items"));
    }
}

/// Outcome of `dbengine_mutate`.
///
/// `invalidated_page_ids` are the pages the driver dropped because the
/// mutation can have made them stale: at least every page read from the
/// mutated object, and every page whose source objects the driver cannot tell
/// (any page, for a driver that does not record them). A page of another
/// object may be kept when the driver knows it cannot be affected (MongoDB
/// keeps the find pages of other collections). The host forgets these pages
/// and a client must not read them again: it re-runs the query that produced
/// them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MutationResult {
    pub affected_rows: u64,
    pub invalidated_page_ids: Vec<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelRequest {
    pub operation_id: OperationId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExportRequest {
    pub operation_id: OperationId,
    pub format: String,
    pub destination: String,
    pub page_id: Option<u64>,
    pub query: Option<QueryRequest>,
    pub options: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportResult {
    pub rows_written: u64,
    pub bytes_written: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriverError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriverErrorEnvelope {
    pub ok: bool,
    pub error: DriverError,
}
