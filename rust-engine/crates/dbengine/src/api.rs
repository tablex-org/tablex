use bson::{Bson, Document};
use std::path::PathBuf;

pub type ConnectionId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OperationId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    PostgreSql,
    Sqlite,
    MongoDb,
    MySql,
    MariaDb,
    Redis,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendFeature {
    Catalog,
    Query,
    Pagination,
    Cancellation,
    Mutations,
    Export,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendCapabilities {
    pub kind: BackendKind,
    pub features: Vec<BackendFeature>,
}

impl BackendCapabilities {
    pub fn new(kind: BackendKind, features: impl IntoIterator<Item = BackendFeature>) -> Self {
        Self {
            kind,
            features: features.into_iter().collect(),
        }
    }

    pub fn supports(&self, feature: BackendFeature) -> bool {
        self.features.contains(&feature)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NativeQuery {
    pub operation_id: OperationId,
    pub request: NativeQueryRequest,
}

#[derive(Debug, Clone, PartialEq)]
pub enum NativeQueryRequest {
    Sql {
        database: String,
        sql: String,
        offset: u64,
        limit: u64,
    },
    MongoFind {
        database: String,
        collection: String,
        filter: Document,
        sort: Document,
        projection: Document,
        offset: u64,
        limit: u64,
    },
    RedisScan {
        pattern: Option<Vec<u8>>,
        cursor: u64,
        count: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeColumn {
    pub id: String,
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeQueryPage {
    pub operation_id: OperationId,
    pub page_id: u64,
    pub columns: Vec<NativeColumn>,
    pub row_count: u64,
    pub total_count: Option<u64>,
    pub total_is_exact: bool,
    pub cancelled: bool,
    pub read_only: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum NativeValue {
    Null,
    Boolean(bool),
    Integer(i64),
    Float(f64),
    Decimal(String),
    Text(String),
    Bytes(Vec<u8>),
    Bson(Bson),
}

#[derive(Debug, Clone, PartialEq)]
pub enum NativeMutationIdentity {
    Sql {
        schema: String,
        table: String,
        key_column: String,
        key: NativeValue,
    },
    MongoId(Bson),
    RedisKey(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct NativeMutationRequest {
    pub operation_id: OperationId,
    pub identity: NativeMutationIdentity,
    pub values: Vec<(String, NativeValue)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeMutationResult {
    pub affected_rows: u64,
    pub invalidated_page_ids: Vec<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeExportFormat {
    Csv,
    Json,
    Sql,
    Xlsx,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeExportRequest {
    pub operation_id: OperationId,
    pub format: NativeExportFormat,
    pub destination: PathBuf,
    pub page_id: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeNavigatorKind {
    Database,
    Schema,
    Table,
    Collection,
    Key,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeNavigatorNode {
    pub id: String,
    pub kind: NativeNavigatorKind,
    pub label: String,
    pub subtitle: Option<String>,
    pub has_children: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeNavigatorPage {
    pub nodes: Vec<NativeNavigatorNode>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeNavigatorRequest {
    pub parent: Option<String>,
    pub database: Option<String>,
    pub cursor: Option<String>,
    pub limit: u64,
}

#[cfg(test)]
mod tests {
    use super::{
        BackendCapabilities, BackendFeature, BackendKind, NativeExportFormat, NativeExportRequest,
        NativeMutationIdentity, NativeMutationRequest, NativeQuery, NativeQueryRequest,
        NativeValue, OperationId,
    };
    use bson::{doc, Bson};
    use std::path::PathBuf;

    #[test]
    fn native_query_keeps_typed_mongo_payload() {
        let query = NativeQuery {
            operation_id: OperationId(7),
            request: NativeQueryRequest::MongoFind {
                database: "app".into(),
                collection: "events".into(),
                filter: doc! { "tenant": Bson::ObjectId([1; 12].into()) },
                sort: doc! { "created_at": -1 },
                projection: doc! { "payload": 1 },
                offset: 0,
                limit: 200,
            },
        };

        let NativeQueryRequest::MongoFind { filter, .. } = query.request else {
            panic!("expected Mongo find request");
        };
        assert!(matches!(filter.get("tenant"), Some(Bson::ObjectId(_))));
    }

    #[test]
    fn native_mutation_preserves_binary_identity_without_json_roundtrip() {
        let request = NativeMutationRequest {
            operation_id: OperationId(8),
            identity: NativeMutationIdentity::RedisKey(vec![0, 1, 255]),
            values: vec![("value".into(), NativeValue::Bytes(vec![0, 255]))],
        };

        assert_eq!(
            request.identity,
            NativeMutationIdentity::RedisKey(vec![0, 1, 255])
        );
        assert_eq!(request.values[0].1, NativeValue::Bytes(vec![0, 255]));
    }

    #[test]
    fn native_capabilities_are_typed_and_do_not_contain_abi_metadata() {
        let capabilities = BackendCapabilities::new(
            BackendKind::MongoDb,
            [BackendFeature::Query, BackendFeature::Pagination],
        );

        assert!(capabilities.supports(BackendFeature::Query));
        assert!(!capabilities.supports(BackendFeature::Mutations));
    }

    #[test]
    fn native_export_uses_typed_format_and_path() {
        let export = NativeExportRequest {
            operation_id: OperationId(9),
            format: NativeExportFormat::Json,
            destination: PathBuf::from("/tmp/result.json"),
            page_id: Some(12),
        };

        assert_eq!(export.format, NativeExportFormat::Json);
        assert_eq!(export.page_id, Some(12));
    }
}
