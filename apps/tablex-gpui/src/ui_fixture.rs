use crate::connections::{ConnectionKind, SavedConnection};
use crate::mongo_admin::{MongoAdminCaps, MongoAdminOp, MongoIndexInfo};
use crate::navigator::{self, KindOfObject, LoadRequest, LoadedChild, ObjectGroup};
use crate::ui_scenario::FixtureKind;
use arrow_array::{
    ArrayRef, BooleanArray, Date32Array, Float64Array, Int64Array, RecordBatch, StringArray,
    UInt64Array,
};
use arrow_schema::{DataType, Field, Schema};
use bson::{oid::ObjectId, Bson, DateTime, Decimal128, Document};
use crate::query::QueryResultBatch;
use crate::staged_sql::{MutationValue, UpdateCellRequest};
use engine_driver_api::{
    CPageLease, CPageSlice, MutationRequest, OperationId, PageColumn, QueryPage, RowField,
    RowFields,
};
use engine_driver_host::PageLease;
use std::collections::HashMap;
use std::ffi::c_void;
use std::str::FromStr;
use std::sync::{Arc, Weak};
use uuid::Uuid;

pub const MONGO_ID: &str = "11111111-1111-4111-8111-111111111111";
pub const POSTGRES_ID: &str = "22222222-2222-4222-8222-222222222222";
pub const SQLITE_ID: &str = "33333333-3333-4333-8333-333333333333";
pub const CLICKHOUSE_ID: &str = "44444444-4444-4444-8444-444444444444";
pub const REDIS_ID: &str = "55555555-5555-4555-8555-555555555555";
/// The extra connections of the `states` fixture, one per sidebar status.
pub const BILLING_ID: &str = "66666666-6666-4666-8666-666666666666";
pub const REPORTING_ID: &str = "77777777-7777-4777-8777-777777777777";
pub const EDGE_ID: &str = "88888888-8888-4888-8888-888888888888";
pub const STAGING_ID: &str = "99999999-9999-4999-8999-999999999999";

#[derive(Debug, Clone)]
struct FixtureRow {
    id: i64,
    name: String,
    status: Option<String>,
    active: bool,
    created: i32,
    score: f64,
    big_number: u64,
    payload: String,
}

#[derive(Debug, Clone)]
pub enum FixtureLoad {
    Children(Vec<LoadedChild>),
    Provider(engine_driver_api::NavigatorPage),
}

pub struct FixtureMongoPage {
    pub page: QueryPage,
    pub lease: PageLease,
}

#[derive(Debug, Clone)]
pub struct UiFixture {
    kind: FixtureKind,
    connections: Vec<SavedConnection>,
    seed_rows: Vec<FixtureRow>,
    rows_by_identity: HashMap<FixtureIdentity, Vec<FixtureRow>>,
    mongo_seed_rows: Vec<Document>,
    mongo_rows_by_identity: HashMap<FixtureIdentity, Vec<Document>>,
    mongo_pages: HashMap<u64, Weak<FixturePageSnapshot>>,
    next_mongo_page_id: u64,
    next_mongo_document_id: u64,
    mongo_catalog: MongoFixtureCatalog,
    /// Every aggregate request the fixture "driver" served (assertions).
    aggregate_calls: Vec<FixtureAggregateCall>,
    /// `(operator, target)` of every `$out` / `$merge` the fixture executed.
    aggregate_writes: Vec<(String, String)>,
    /// How long the fixture "server" takes to answer aggregate previews and
    /// explains (zero = answers synchronously).
    aggregate_latency: std::time::Duration,
    /// Previews / explains the app cancelled while they were in flight.
    aggregate_cancels: u32,
    /// `(pipeline text, destination)` of every Export Data the fixture served.
    aggregate_exports: Vec<(String, std::path::PathBuf)>,
    /// When set, the next Export Data fails with this message (error paths).
    aggregate_export_failure: Option<String>,
    /// Whether Mongo script runs are served by the fixture (opt-in: the
    /// default scenario refuses them, which existing flows rely on).
    mongo_scripts_enabled: bool,
    /// The Redis stand-in (see `ui_fixture_redis`); `None` until a Redis
    /// connection is added.
    redis: Option<crate::ui_fixture_redis::RedisFixture>,
}

/// One served `mongo_aggregate` / `mongo_aggregate_explain` request.
#[derive(Debug, Clone, PartialEq)]
pub struct FixtureAggregateCall {
    pub kind: &'static str,
    pub payload: serde_json::Value,
    /// The pipeline after the driver rules were applied (preview `$limit`,
    /// stripped writes, page window).
    pub pipeline: Vec<Document>,
}

/// Stateful stand-in for a MongoDB server's catalog: the scenario fixture
/// answers the admin operations (`crate::mongo_admin`) the way the driver
/// does, including its refusals, so UI flows can be driven end to end.
#[derive(Debug, Clone)]
struct MongoFixtureCatalog {
    databases: Vec<String>,
    collections: HashMap<String, Vec<String>>,
    indexes: HashMap<(String, String), Vec<MongoIndexInfo>>,
    features: Vec<String>,
    /// `kind` of every admin mutation applied, in order (assertions).
    operations: Vec<String>,
}

impl MongoFixtureCatalog {
    fn new() -> Self {
        let databases: Vec<String> = (0..8).map(|index| format!("fixture_db_{index}")).collect();
        let collections = databases
            .iter()
            .map(|database| {
                (
                    database.clone(),
                    (0..25)
                        .map(|index| format!("fixture_collection_{index:02}"))
                        .collect(),
                )
            })
            .collect();
        Self {
            databases,
            collections,
            indexes: HashMap::new(),
            features: vec![
                engine_driver_api::Capability::AdminCollection.as_str().into(),
                engine_driver_api::Capability::AdminIndexes.as_str().into(),
                engine_driver_api::Capability::AdminDatabase.as_str().into(),
            ],
            operations: Vec::new(),
        }
    }

    fn default_indexes() -> Vec<MongoIndexInfo> {
        let index = |name: &str, field: &str, unique: bool, ttl: Option<i64>| MongoIndexInfo {
            name: name.into(),
            keys: vec![(field.into(), "1".into())],
            unique,
            sparse: false,
            ttl_seconds: ttl,
            partial_filter: None,
        };
        vec![
            index("_id_", "_id", true, None),
            index("email_1", "email", true, None),
            index("created_at_1", "created_at", false, Some(86_400)),
        ]
    }

    fn collection_names(&self, database: &str) -> Result<&Vec<String>, String> {
        self.collections
            .get(database)
            .ok_or_else(|| format!("Database '{database}' does not exist"))
    }

    fn require_collection(&self, database: &str, collection: &str) -> Result<(), String> {
        if self.collection_names(database)?.iter().any(|name| name == collection) {
            Ok(())
        } else {
            Err(format!("Collection '{collection}' does not exist"))
        }
    }

    fn indexes_mut(&mut self, database: &str, collection: &str) -> &mut Vec<MongoIndexInfo> {
        self.indexes
            .entry((database.into(), collection.into()))
            .or_insert_with(Self::default_indexes)
    }

    fn list_indexes(&self, database: &str, collection: &str) -> Result<Vec<MongoIndexInfo>, String> {
        self.require_collection(database, collection)?;
        Ok(self
            .indexes
            .get(&(database.to_string(), collection.to_string()))
            .cloned()
            .unwrap_or_else(Self::default_indexes))
    }

    fn apply(&mut self, database: &str, op: &MongoAdminOp, documents: u64) -> Result<u64, String> {
        use crate::mongo_admin::{is_protected_database, is_system_collection};
        let affected = match op {
            MongoAdminOp::DropCollection { collection } => {
                if is_system_collection(collection) {
                    return Err("System collections cannot be modified".into());
                }
                self.require_collection(database, collection)?;
                if let Some(names) = self.collections.get_mut(database) {
                    names.retain(|name| name != collection);
                }
                self.indexes.remove(&(database.to_string(), collection.clone()));
                1
            }
            MongoAdminOp::RenameCollection { from, to } => {
                crate::mongo_admin::validate_new_collection_name(database, to)?;
                if from == to {
                    return Err("The new name must differ from the current name".into());
                }
                self.require_collection(database, from)?;
                if self.require_collection(database, to).is_ok() {
                    return Err(format!("Collection '{to}' already exists"));
                }
                if let Some(names) = self.collections.get_mut(database) {
                    names.retain(|name| name != from);
                    names.push(to.clone());
                    names.sort();
                }
                if let Some(indexes) = self.indexes.remove(&(database.to_string(), from.clone())) {
                    self.indexes.insert((database.to_string(), to.clone()), indexes);
                }
                1
            }
            MongoAdminOp::CloneCollection {
                from,
                to,
                include_indexes,
            } => {
                crate::mongo_admin::validate_new_collection_name(database, to)?;
                if from == to {
                    return Err("The clone name must differ from the source collection".into());
                }
                self.require_collection(database, from)?;
                if self.require_collection(database, to).is_ok() {
                    return Err(format!("Collection '{to}' already exists"));
                }
                let source = self.list_indexes(database, from)?;
                if let Some(names) = self.collections.get_mut(database) {
                    names.push(to.clone());
                    names.sort();
                }
                let copied = source
                    .into_iter()
                    .filter(|index| *include_indexes || index.is_id())
                    .collect();
                self.indexes.insert((database.to_string(), to.clone()), copied);
                documents
            }
            MongoAdminOp::CreateIndex { collection, index } => {
                if is_system_collection(collection) {
                    return Err("System collections cannot be modified".into());
                }
                self.require_collection(database, collection)?;
                let name = index.effective_name();
                let existing = self.indexes_mut(database, collection);
                if existing.iter().any(|candidate| candidate.name == name) {
                    return Err(format!("Index '{name}' already exists"));
                }
                existing.push(MongoIndexInfo {
                    name,
                    keys: index
                        .keys
                        .iter()
                        .map(|(field, key_type)| {
                            let label = match key_type {
                                crate::mongo_admin::IndexKeyType::Ascending => "1",
                                crate::mongo_admin::IndexKeyType::Descending => "-1",
                                crate::mongo_admin::IndexKeyType::Text => "text",
                                crate::mongo_admin::IndexKeyType::Hashed => "hashed",
                                crate::mongo_admin::IndexKeyType::Geo2dSphere => "2dsphere",
                            };
                            (field.clone(), label.to_string())
                        })
                        .collect(),
                    unique: index.unique,
                    sparse: index.sparse,
                    ttl_seconds: index.expire_after_seconds.map(i64::from),
                    partial_filter: None,
                });
                1
            }
            MongoAdminOp::DropIndex { collection, name } => {
                if name == "_id_" {
                    return Err("The _id_ index cannot be dropped".into());
                }
                self.require_collection(database, collection)?;
                let existing = self.indexes_mut(database, collection);
                let before = existing.len();
                existing.retain(|candidate| candidate.name != *name);
                if existing.len() == before {
                    return Err(format!("Index '{name}' does not exist"));
                }
                1
            }
            MongoAdminOp::CreateDatabase => {
                if self.databases.iter().any(|name| name == database) {
                    return Err(format!("Database '{database}' already exists"));
                }
                // Like the driver: the database exists once its first collection does.
                self.databases.push(database.to_string());
                self.databases.sort();
                self.collections.insert(database.to_string(), vec!["tablex_init".into()]);
                1
            }
            MongoAdminOp::DropDatabase => {
                if is_protected_database(database) {
                    return Err(format!("The '{database}' database cannot be dropped"));
                }
                if !self.databases.iter().any(|name| name == database) {
                    return Err(format!("Database '{database}' does not exist"));
                }
                self.databases.retain(|name| name != database);
                self.collections.remove(database);
                self.indexes.retain(|(db, _), _| db != database);
                1
            }
        };
        self.operations.push(op.mutation(database, OperationId(0)).kind);
        Ok(affected)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FixtureIdentity {
    connection: String,
    database: String,
    schema: String,
    object: String,
}

impl UiFixture {
    pub fn new(kind: FixtureKind) -> Self {
        // Capture runs opt in with TABLEX_FIXTURE_CLICKHOUSE=1: the extra
        // sidebar connection would shift every fixed-count sidebar proof.
        Self::with_clickhouse(
            kind,
            std::env::var("TABLEX_FIXTURE_CLICKHOUSE").as_deref() == Ok("1"),
        )
    }

    /// `new`, with the ClickHouse sidebar connection added when asked.
    pub fn with_clickhouse(kind: FixtureKind, clickhouse: bool) -> Self {
        let mut fixture = Self::sample(kind);
        if std::env::var("TABLEX_FIXTURE_REDIS").as_deref() == Ok("1") {
            fixture.add_redis();
        }
        if clickhouse {
            fixture.connections.push(connection(
                CLICKHOUSE_ID,
                "ClickHouse fixture",
                ConnectionKind::Clickhouse,
                "clickhouse://fixture.invalid:8123/default",
            ));
        }
        fixture
    }

    fn sample(kind: FixtureKind) -> Self {
        let mut fixture = Self::sample_rows(kind);
        if kind == FixtureKind::States {
            fixture.connections.extend([
                connection(BILLING_ID, "Billing", ConnectionKind::Postgres, "postgres://billing.invalid/billing"),
                connection(REPORTING_ID, "Reporting", ConnectionKind::Mariadb, "mariadb://reporting.invalid/reporting"),
                connection(EDGE_ID, "Edge cache (behind SSH)", ConnectionKind::Mongodb, "mongodb://edge.invalid/cache"),
                connection(STAGING_ID, "Staging", ConnectionKind::Mysql, "mysql://staging.invalid/staging"),
            ]);
        }
        fixture
    }

    fn sample_rows(kind: FixtureKind) -> Self {
        Self {
            kind,
            connections: vec![
                connection(
                    MONGO_ID,
                    "Mongo fixture",
                    ConnectionKind::Mongodb,
                    "mongodb://fixture.invalid/analytics",
                ),
                connection(
                    POSTGRES_ID,
                    "Postgres fixture",
                    ConnectionKind::Postgres,
                    "postgres://fixture.invalid/warehouse",
                ),
                connection(
                    SQLITE_ID,
                    "SQLite fixture",
                    ConnectionKind::Sqlite,
                    "sqlite://fixture.db",
                ),
            ],
            seed_rows: (0..240).map(FixtureRow::new).collect(),
            rows_by_identity: HashMap::new(),
            mongo_seed_rows: (0..240).map(mongo_document).collect(),
            mongo_rows_by_identity: HashMap::new(),
            mongo_pages: HashMap::new(),
            next_mongo_page_id: 1,
            next_mongo_document_id: 241,
            mongo_catalog: MongoFixtureCatalog::new(),
            aggregate_calls: Vec::new(),
            aggregate_writes: Vec::new(),
            aggregate_latency: std::time::Duration::ZERO,
            aggregate_cancels: 0,
            aggregate_exports: Vec::new(),
            aggregate_export_failure: None,
            // Capture runs opt in with TABLEX_FIXTURE_SCRIPTS=1.
            mongo_scripts_enabled: std::env::var("TABLEX_FIXTURE_SCRIPTS").as_deref() == Ok("1"),
            redis: None,
        }
    }

    /// Adds the Redis sidebar connection with its in-memory server. Opt-in
    /// (`TABLEX_FIXTURE_REDIS=1`, the `redis` scenario, or a test): the extra
    /// connection would shift every fixed-count sidebar proof.
    pub fn add_redis(&mut self) {
        if self.redis.is_none() {
            self.connections.push(connection(
                REDIS_ID,
                "Redis fixture",
                ConnectionKind::Redis,
                "redis://fixture.invalid:6379/0",
            ));
            self.redis = Some(crate::ui_fixture_redis::RedisFixture::sample());
        }
    }

    pub fn redis(&self) -> Option<&crate::ui_fixture_redis::RedisFixture> {
        self.redis.as_ref()
    }

    pub fn redis_mut(&mut self) -> Option<&mut crate::ui_fixture_redis::RedisFixture> {
        self.redis.as_mut()
    }

    /// Test hook: a second Mongo connection (connection-switch flows).
    #[cfg(test)]
    pub fn add_mongo_connection(&mut self, id: &str, name: &str) -> SavedConnection {
        let saved = connection(id, name, ConnectionKind::Mongodb, "mongodb://second.invalid/other");
        self.connections.push(saved.clone());
        saved
    }

    /// Test hook: make every catalog load fail like the `error` fixture.
    #[cfg(test)]
    pub fn set_catalog_failing(&mut self, failing: bool) {
        self.kind = if failing { FixtureKind::Error } else { FixtureKind::Sample };
    }

    pub fn mongo_scripts_enabled(&self) -> bool {
        self.mongo_scripts_enabled
    }

    #[cfg(test)]
    pub fn set_mongo_scripts_enabled(&mut self, enabled: bool) {
        self.mongo_scripts_enabled = enabled;
    }

    #[cfg(test)]
    pub fn aggregate_exports(&self) -> &[(String, std::path::PathBuf)] {
        &self.aggregate_exports
    }

    #[cfg(test)]
    pub fn fail_next_aggregate_export(&mut self, message: &str) {
        self.aggregate_export_failure = Some(message.into());
    }

    /// Stand-in for the driver's `export.mongo_aggregate`: the whole result
    /// of the pipeline (no window), columns = union of every document's keys,
    /// written through the same `TableExporter` the app uses elsewhere.
    pub fn export_mongo_aggregate(
        &mut self,
        connection: &str,
        database: &str,
        collection: &str,
        payload: &serde_json::Value,
        format: &str,
        destination: &std::path::Path,
    ) -> Result<(u64, u64), String> {
        use crate::mongo_aggregate as engine;
        if let Some(message) = self.aggregate_export_failure.take() {
            return Err(message);
        }
        if self.connection_kind(connection) != Some(ConnectionKind::Mongodb) {
            return Err("Mongo fixture connection is required".into());
        }
        let request = engine::parse_request_payload(payload).map_err(|error| error.to_string())?;
        if let Some(write) = engine::final_write_stage(&request.stages) {
            return Err(format!(
                "Export Data cannot run a pipeline that writes ({} to {}).",
                write.operator, write.target
            ));
        }
        let identity = FixtureIdentity {
            connection: connection.into(),
            database: database.into(),
            schema: String::new(),
            object: collection.into(),
        };
        let source = self
            .mongo_rows_by_identity
            .get(&identity)
            .cloned()
            .unwrap_or_else(|| self.mongo_seed_rows.clone());
        let documents = fixture_run_pipeline(source, &request.stages)?;
        let mut columns: Vec<String> = Vec::new();
        for document in &documents {
            for key in document.keys() {
                if !columns.contains(key) {
                    columns.push(key.clone());
                }
            }
        }
        let format = match format {
            "csv" => engine_export::ExportFormat::Csv,
            "json" => engine_export::ExportFormat::Json,
            "xlsx" => engine_export::ExportFormat::Xlsx,
            other => return Err(format!("unsupported export format: {other}")),
        };
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let mut exporter = engine_export::TableExporter::create(
            destination,
            format,
            engine_export::ExportDialect::Mongo,
            collection.to_string(),
            columns.clone(),
        )?;
        let rows: Vec<Vec<String>> = documents
            .iter()
            .map(|document| {
                columns
                    .iter()
                    .map(|column| document.get(column).map(mongo_display_text).unwrap_or_default())
                    .collect()
            })
            .collect();
        exporter.write_rows(&rows)?;
        let count = exporter.finish()? as u64;
        let bytes = std::fs::metadata(destination).map(|meta| meta.len()).unwrap_or(0);
        self.aggregate_exports.push((
            payload["pipeline"].as_str().unwrap_or_default().to_string(),
            destination.to_path_buf(),
        ));
        Ok((count, bytes))
    }

    pub fn aggregate_latency(&self) -> std::time::Duration {
        self.aggregate_latency
    }

    #[cfg(test)]
    /// Test / capture hook: delay aggregate preview and explain answers.
    pub fn set_aggregate_latency(&mut self, latency: std::time::Duration) {
        self.aggregate_latency = latency;
    }

    #[cfg(test)]
    pub fn aggregate_cancels(&self) -> u32 {
        self.aggregate_cancels
    }

    pub fn note_aggregate_cancel(&mut self) {
        self.aggregate_cancels += 1;
    }

    #[cfg(test)]
    pub fn aggregate_calls(&self) -> &[FixtureAggregateCall] {
        &self.aggregate_calls
    }

    #[cfg(test)]
    pub fn aggregate_writes(&self) -> &[(String, String)] {
        &self.aggregate_writes
    }

    /// Stand-in for the MongoDB driver's `mongo_aggregate` (`explain` =
    /// false) and `mongo_aggregate_explain` kinds. The request is decoded
    /// and shaped by the same module the driver compiles
    /// (`mongo_aggregate`), so preview caps, write stripping, paging and the
    /// write confirmation rule are the driver's; only stage evaluation is
    /// the fixture's own (see [`fixture_run_pipeline`]).
    #[allow(clippy::too_many_arguments)] // mirrors `query_mongo_page`
    pub fn aggregate_mongo_page(
        &mut self,
        connection: &str,
        database: &str,
        collection: &str,
        explain: bool,
        payload: &serde_json::Value,
        offset: u64,
        limit: i64,
    ) -> Result<FixtureMongoPage, String> {
        use crate::mongo_aggregate as engine;
        if self.kind == FixtureKind::Error {
            return Err("fixture query error".into());
        }
        if self.connection_kind(connection) != Some(ConnectionKind::Mongodb) {
            return Err("Mongo fixture connection is required".into());
        }
        let request = engine::parse_request_payload(payload).map_err(|error| error.to_string())?;
        let identity = FixtureIdentity {
            connection: connection.into(),
            database: database.into(),
            schema: String::new(),
            object: collection.into(),
        };
        let mut source = self
            .mongo_rows_by_identity
            .get(&identity)
            .cloned()
            .unwrap_or_else(|| self.mongo_seed_rows.clone());
        if self.kind == FixtureKind::Empty {
            source.clear();
        }
        let (kind, pipeline, documents, total_count, total_is_exact) = if explain {
            let verbosity = payload
                .get("verbosity")
                .and_then(serde_json::Value::as_str)
                .map_or(Some(engine::ExplainVerbosity::QueryPlanner), engine::ExplainVerbosity::parse)
                .ok_or("unknown explain verbosity")?;
            let pipeline = engine::explain_pipeline(&request.stages, verbosity);
            let mut plan = bson::doc! {
                "queryPlanner": {
                    "namespace": format!("{database}.{collection}"),
                    "winningPlan": { "stage": "COLLSCAN", "direction": "forward" },
                },
                "stages": pipeline.len() as i64,
            };
            if verbosity.executes() {
                let returned = fixture_run_pipeline(source, &pipeline)?.len() as i64;
                plan.insert(
                    "executionStats",
                    bson::doc! { "nReturned": returned, "executionTimeMillis": 3_i64 },
                );
            }
            plan.insert("verbosity", verbosity.as_str());
            ("mongo_aggregate_explain", pipeline, vec![plan], 1, true)
        } else {
            match request.mode {
                engine::RunMode::Preview => {
                    let pipeline = engine::preview_pipeline(
                        &request.stages,
                        request.stage_index,
                        request.sample_size,
                    );
                    let documents = fixture_run_pipeline(source, &pipeline)?;
                    let count = documents.len() as u64;
                    ("mongo_aggregate", pipeline, documents, count, false)
                }
                engine::RunMode::Full => {
                    let limit = u64::try_from(limit.max(1)).unwrap_or(1).min(200);
                    let plan = engine::full_pipeline(&request.stages, offset, limit);
                    if let Some(write) = &plan.write {
                        if !request.confirm_write {
                            return Err(format!(
                                "query.write_requires_confirmation: This pipeline writes to {} with {}; confirm to run it.",
                                write.target, write.operator
                            ));
                        }
                        self.aggregate_writes
                            .push((write.operator.to_string(), write.target.clone()));
                        let summary = bson::doc! {
                            "stage": write.operator,
                            "target": write.target.clone(),
                            "acknowledged": true,
                        };
                        ("mongo_aggregate", plan.pipeline, vec![summary], 1, true)
                    } else {
                        let mut documents = fixture_run_pipeline(source, &plan.pipeline)?;
                        let has_more = documents.len() as u64 > plan.page_limit;
                        documents.truncate(plan.page_limit as usize);
                        let total = offset
                            .saturating_add(documents.len() as u64)
                            .saturating_add(u64::from(has_more));
                        ("mongo_aggregate", plan.pipeline, documents, total, !has_more)
                    }
                }
            }
        };
        self.aggregate_calls.push(FixtureAggregateCall {
            kind,
            payload: payload.clone(),
            pipeline,
        });
        let page_id = self.next_mongo_page_id;
        self.next_mongo_page_id = self.next_mongo_page_id.saturating_add(1);
        let row_count = documents.len() as u64;
        let (columns, lease, snapshot) = mongo_page_lease(&documents, documents.clone(), page_id)?;
        self.mongo_pages
            .retain(|_, snapshot| snapshot.upgrade().is_some());
        self.mongo_pages.insert(page_id, Arc::downgrade(&snapshot));
        Ok(FixtureMongoPage {
            page: QueryPage {
                operation_id: OperationId(page_id),
                page_id,
                columns,
                row_count,
                total_count: Some(total_count),
                total_is_exact,
                flags: engine_driver_api::PageFlags {
                    truncated: false,
                    cancelled: false,
                    read_only: true,
                },
            },
            lease,
        })
    }

    /// Admin features the fixture's Mongo "driver" advertises.
    pub fn mongo_admin_caps(&self) -> MongoAdminCaps {
        MongoAdminCaps::from_features(self.mongo_catalog.features.iter().map(String::as_str))
    }

    /// Test hook: restrict the advertised admin features (capability gating).
    #[cfg(test)]
    pub fn set_mongo_admin_features(&mut self, features: &[&str]) {
        self.mongo_catalog.features = features.iter().map(|feature| feature.to_string()).collect();
    }

    /// Admin mutation kinds the fixture applied, oldest first.
    #[cfg(test)]
    pub fn mongo_admin_operations(&self) -> &[String] {
        &self.mongo_catalog.operations
    }

    pub fn apply_mongo_admin(
        &mut self,
        connection: &str,
        database: &str,
        op: &MongoAdminOp,
    ) -> Result<u64, String> {
        if self.connection_kind(connection) != Some(ConnectionKind::Mongodb) {
            return Err("Mongo fixture connection is required".into());
        }
        let documents = self.mongo_seed_rows.len() as u64;
        self.mongo_catalog.apply(database, op, documents)
    }

    pub fn list_mongo_indexes(
        &self,
        connection: &str,
        database: &str,
        collection: &str,
    ) -> Result<Vec<MongoIndexInfo>, String> {
        if self.connection_kind(connection) != Some(ConnectionKind::Mongodb) {
            return Err("Mongo fixture connection is required".into());
        }
        self.mongo_catalog.list_indexes(database, collection)
    }

    pub fn connections(&self) -> &[SavedConnection] {
        &self.connections
    }

    pub fn catalog_columns(&self) -> Vec<crate::catalog::CatalogColumn> {
        [
            ("id", "int64", false, true),
            ("name", "text", false, false),
            ("status", "text", true, false),
            ("active", "boolean", false, false),
            ("created", "date", false, false),
            ("score", "float", false, false),
            ("big_number", "bigint", false, false),
            ("payload", "json", false, false),
        ]
        .into_iter()
        .map(
            |(name, data_type, nullable, primary_key)| crate::catalog::CatalogColumn {
                name: name.into(),
                data_type: data_type.into(),
                nullable,
                primary_key,
                is_generated: Some(false),
                default_expression: None,
                auto_increment: Some(primary_key && name == "id"),
                comment: None,
                attributes: None,
            },
        )
        .collect()
    }

    pub fn load(&self, request: &LoadRequest) -> Result<FixtureLoad, String> {
        if self.kind == FixtureKind::Error {
            return Err("fixture catalog error".into());
        }
        match request {
            LoadRequest::Databases { connection } => match self.connection_kind(connection) {
                Some(ConnectionKind::Postgres) => Ok(FixtureLoad::Children(vec![
                    navigator::database_child(connection, "warehouse"),
                    navigator::database_child(connection, "reporting"),
                ])),
                Some(ConnectionKind::Sqlite) => {
                    Ok(FixtureLoad::Children(vec![navigator::database_child(
                        connection, "main",
                    )]))
                }
                Some(ConnectionKind::Clickhouse) => Ok(FixtureLoad::Children(vec![
                    navigator::database_child(connection, "default"),
                    navigator::database_child(connection, "analytics"),
                ])),
                Some(ConnectionKind::Mongodb | ConnectionKind::Redis) => {
                    Err("this connection uses provider catalog loading".into())
                }
                _ => Err("unknown fixture connection".into()),
            },
            LoadRequest::Schemas {
                connection,
                database,
            } => match self.connection_kind(connection) {
                Some(ConnectionKind::Postgres) => Ok(FixtureLoad::Children(
                    ["public", "analytics"]
                        .into_iter()
                        .map(|schema| navigator::schema_child(connection, database, schema))
                        .collect(),
                )),
                Some(ConnectionKind::Sqlite) => {
                    Ok(FixtureLoad::Children(vec![navigator::schema_child(
                        connection, database, "main",
                    )]))
                }
                // No schema level: the one schema is named like its database.
                Some(ConnectionKind::Clickhouse) => {
                    Ok(FixtureLoad::Children(vec![navigator::schema_child(
                        connection, database, database,
                    )]))
                }
                Some(ConnectionKind::Mongodb) => Err("MongoDB has no schema level".into()),
                _ => Err("unknown fixture connection".into()),
            },
            LoadRequest::Objects {
                connection,
                database,
                schema,
                group,
            } => {
                // Functions, triggers, ...: the objects of a PostgreSQL / MySQL dialect.
                if let Some(kind) = group.schema_object_kind().filter(|_| !group.is_table_like()) {
                    return Ok(FixtureLoad::Children(
                        schema_object_names(kind)
                            .iter()
                            .map(|&(name, arguments, table)| {
                                let reference = navigator::SchemaObjectRef {
                                    database: Some(database.clone()),
                                    schema: Some(schema.clone()),
                                    kind,
                                    name: name.into(),
                                    arguments: arguments.map(str::to_owned),
                                    table: table.map(str::to_owned),
                                    identity: None,
                                };
                                navigator::schema_object_child(
                                    connection,
                                    database,
                                    schema,
                                    *group,
                                    &schema_object_label(&reference),
                                    &schema_object_label(&reference),
                                    reference,
                                )
                            })
                            .collect(),
                    ));
                }
                let names: Vec<String> = if *group == ObjectGroup::MaterializedViews {
                    vec!["monthly_revenue".into()]
                } else if *group == ObjectGroup::Views {
                    vec!["daily_summary".into(), "long_view_name_東京".into()]
                } else {
                    (0..20)
                        .map(|index| {
                            if index == 0 {
                                "orders".into()
                            } else {
                                format!("fixture_table_{index:02}")
                            }
                        })
                        .collect()
                };
                Ok(FixtureLoad::Children(
                    names
                        .into_iter()
                        .map(|name| {
                            navigator::object_child(
                                connection,
                                database,
                                schema,
                                &name,
                                match group {
                                    ObjectGroup::Views => KindOfObject::View,
                                    ObjectGroup::MaterializedViews => KindOfObject::MaterializedView,
                                    _ => KindOfObject::Table,
                                },
                            )
                        })
                        .collect(),
                ))
            }
            LoadRequest::Columns {
                connection,
                database,
                schema,
                object,
            } => Ok(FixtureLoad::Children(
                [
                    "id",
                    "name",
                    "status",
                    "active",
                    "created",
                    "score",
                    "big_number",
                    "payload",
                ]
                .into_iter()
                .map(|column| navigator::column_child(connection, database, schema, object, column))
                .collect(),
            )),
            LoadRequest::Provider {
                connection,
                parent,
                payload,
                cursor,
                limit,
            } if self.connection_kind(connection) == Some(ConnectionKind::Redis) => {
                let redis = self.redis.as_ref().ok_or("the Redis fixture is not enabled")?;
                Ok(FixtureLoad::Provider(redis.navigator_page(
                    parent.as_deref(),
                    payload.as_ref(),
                    cursor.as_deref(),
                    *limit,
                )?))
            }
            LoadRequest::Provider {
                connection,
                parent,
                payload,
                cursor,
                limit,
            } => Ok(FixtureLoad::Provider(provider_page(
                &self.mongo_catalog,
                connection,
                parent.as_deref(),
                payload.as_ref(),
                cursor.as_deref(),
                *limit,
            )?)),
        }
    }

    pub fn query_page(
        &self,
        connection: &str,
        _database: &str,
        _schema: &str,
        _object: &str,
        offset: u64,
        limit: i64,
    ) -> Result<QueryResultBatch, String> {
        self.query_page_with_options(
            connection, _database, _schema, _object, offset, limit, None, None,
        )
    }

    #[allow(clippy::too_many_arguments)] // each parameter is one input of the operation; a parameter struct would only rename the list
    pub fn query_page_with_options(
        &self,
        connection: &str,
        database: &str,
        schema: &str,
        object: &str,
        offset: u64,
        limit: i64,
        filter: Option<&str>,
        sort: Option<&str>,
    ) -> Result<QueryResultBatch, String> {
        if self.kind == FixtureKind::Error {
            return Err("fixture query error".into());
        }
        if self.kind == FixtureKind::Empty {
            return Ok(QueryResultBatch {
                batches: vec![Arc::new(record_batch(&[])?)],
                total_count: 0,
            });
        }
        if self.connection_kind(connection).is_none() {
            return Err("unknown fixture connection".into());
        }
        let filter = filter
            .filter(|filter| !filter.trim().is_empty())
            .map(|filter| {
                serde_json::from_str::<serde_json::Value>(filter).map_err(|error| error.to_string())
            })
            .transpose()?;
        let sort = sort
            .filter(|sort| !sort.trim().is_empty())
            .map(|sort| {
                serde_json::from_str::<serde_json::Value>(sort).map_err(|error| error.to_string())
            })
            .transpose()?;
        let identity = FixtureIdentity {
            connection: connection.into(),
            database: database.into(),
            schema: schema.into(),
            object: object.into(),
        };
        let rows = self
            .rows_by_identity
            .get(&identity)
            .map(Vec::as_slice)
            .unwrap_or(&self.seed_rows);
        let mut indices = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| {
                filter
                    .as_ref()
                    .is_none_or(|filter| fixture_matches_filter(row, filter))
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        if let Some(sort) = sort.as_ref() {
            fixture_sort_indices(&mut indices, rows, sort)?;
        }
        let total_count = indices.len() as i64;
        let start = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(indices.len());
        let end = start
            .saturating_add(limit.max(1) as usize)
            .min(indices.len());
        let rows = indices[start..end]
            .iter()
            .map(|index| rows[*index].clone())
            .collect::<Vec<_>>();
        Ok(QueryResultBatch {
            batches: if rows.is_empty() {
                Vec::new()
            } else {
                vec![Arc::new(record_batch(&rows)?)]
            },
            total_count,
        })
    }

    /// Fixture equivalent of a `sqlTable` tab run: a small evaluator for the
    /// WHERE (comparisons, `AND` / `OR` / `NOT`, parentheses, `IS [NOT] NULL`,
    /// `[NOT] LIKE .. ESCAPE`, `[NOT] IN (..)`, `[NOT] BETWEEN`) and ORDER BY
    /// (`col [ASC|DESC], ...`) fragments. Unknown columns fail like an engine
    /// error so the bar's error state can be exercised.
    pub fn query_sql_table_page(
        &self,
        table: (&str, &str, &str, &str),
        offset: u64,
        limit: i64,
        fragments: (&str, &str),
    ) -> Result<QueryResultBatch, String> {
        let (connection, database, schema, object) = table;
        let (where_clause, order_by) = fragments;
        if self.kind == FixtureKind::Error {
            return Err("fixture query error".into());
        }
        if self.kind == FixtureKind::Empty {
            return Ok(QueryResultBatch {
                batches: vec![Arc::new(record_batch(&[])?)],
                total_count: 0,
            });
        }
        if self.connection_kind(connection).is_none() {
            return Err("unknown fixture connection".into());
        }
        let condition = fixture_parse_where(where_clause)?;
        let terms = fixture_parse_order(order_by)?;
        let identity = FixtureIdentity {
            connection: connection.into(),
            database: database.into(),
            schema: schema.into(),
            object: object.into(),
        };
        let rows = self
            .rows_by_identity
            .get(&identity)
            .map(Vec::as_slice)
            .unwrap_or(&self.seed_rows);
        let mut indices = Vec::new();
        for (index, row) in rows.iter().enumerate() {
            let keep = match &condition {
                Some(condition) => condition.eval(row)? == Some(true),
                None => true,
            };
            if keep {
                indices.push(index);
            }
        }
        if !terms.is_empty() {
            let mut keyed = Vec::with_capacity(indices.len());
            for index in indices {
                let mut key = Vec::with_capacity(terms.len());
                for (column, _) in &terms {
                    key.push(fixture_value(&rows[index], column)?);
                }
                keyed.push((index, key));
            }
            keyed.sort_by(|(_, left), (_, right)| {
                for (position, (_, descending)) in terms.iter().enumerate() {
                    let ordering = left[position].order(&right[position]);
                    if ordering != std::cmp::Ordering::Equal {
                        return if *descending { ordering.reverse() } else { ordering };
                    }
                }
                std::cmp::Ordering::Equal
            });
            indices = keyed.into_iter().map(|(index, _)| index).collect();
        }
        let total_count = indices.len() as i64;
        let start = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(indices.len());
        let end = start
            .saturating_add(limit.max(1) as usize)
            .min(indices.len());
        let rows = indices[start..end]
            .iter()
            .map(|index| rows[*index].clone())
            .collect::<Vec<_>>();
        Ok(QueryResultBatch {
            batches: if rows.is_empty() {
                Vec::new()
            } else {
                vec![Arc::new(record_batch(&rows)?)]
            },
            total_count,
        })
    }

    pub fn apply_sql_updates(
        &mut self,
        connection: &str,
        database: &str,
        schema: &str,
        object: &str,
        updates: &[UpdateCellRequest],
    ) -> Result<usize, String> {
        if self.connection_kind(connection).is_none() {
            return Err("unknown fixture connection".into());
        }
        let identity = FixtureIdentity {
            connection: connection.into(),
            database: database.into(),
            schema: schema.into(),
            object: object.into(),
        };
        let source = self
            .rows_by_identity
            .get(&identity)
            .map(Vec::as_slice)
            .unwrap_or(&self.seed_rows);
        for update in updates {
            if update.database != database || update.schema != schema || update.table != object {
                return Err("fixture update context does not match active object".into());
            }
            validate_update(update, source)?;
        }
        let seed_rows = self.seed_rows.clone();
        let rows = self.rows_by_identity.entry(identity).or_insert(seed_rows);
        for update in updates {
            let id = mutation_i64(&update.key_value).expect("validated fixture row key");
            let row = rows
                .iter_mut()
                .find(|row| row.id == id)
                .expect("validated fixture row");
            apply_update(row, update);
        }
        Ok(updates.len())
    }

    #[allow(clippy::too_many_arguments)] // each parameter is one input of the operation; a parameter struct would only rename the list
    pub fn query_mongo_page(
        &mut self,
        connection: &str,
        database: &str,
        collection: &str,
        offset: u64,
        limit: i64,
        filter: Option<&str>,
        sort: Option<&str>,
    ) -> Result<FixtureMongoPage, String> {
        if self.kind == FixtureKind::Error {
            return Err("fixture query error".into());
        }
        if self.connection_kind(connection) != Some(ConnectionKind::Mongodb) {
            return Err("Mongo fixture connection is required".into());
        }
        let identity = FixtureIdentity {
            connection: connection.into(),
            database: database.into(),
            schema: String::new(),
            object: collection.into(),
        };
        let source = self
            .mongo_rows_by_identity
            .get(&identity)
            .cloned()
            .unwrap_or_else(|| self.mongo_seed_rows.clone());
        let filter = filter
            .filter(|value| !value.trim().is_empty())
            .map(|value| {
                serde_json::from_str::<serde_json::Value>(value).map_err(|error| error.to_string())
            })
            .transpose()?;
        let sort = sort
            .filter(|value| !value.trim().is_empty())
            .map(|value| {
                serde_json::from_str::<serde_json::Value>(value).map_err(|error| error.to_string())
            })
            .transpose()?;
        let mut indices = source
            .iter()
            .enumerate()
            .filter(|(_, document)| {
                filter
                    .as_ref()
                    .is_none_or(|filter| mongo_matches_filter(document, filter))
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        if let Some(sort) = sort.as_ref() {
            mongo_sort_indices(&mut indices, &source, sort)?;
        }
        if self.kind == FixtureKind::Empty {
            indices.clear();
        }
        let total_count = indices.len() as u64;
        let start = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(indices.len());
        let page_limit = usize::try_from(limit.max(0)).unwrap_or(usize::MAX);
        let end = start.saturating_add(page_limit).min(indices.len());
        let documents = indices[start..end]
            .iter()
            .map(|index| source[*index].clone())
            .collect::<Vec<_>>();
        let page_id = self.next_mongo_page_id;
        self.next_mongo_page_id = self.next_mongo_page_id.saturating_add(1);
        let (columns, lease, snapshot) = mongo_page_lease(&source, documents, page_id)?;
        self.mongo_pages
            .retain(|_, snapshot| snapshot.upgrade().is_some());
        self.mongo_pages.insert(page_id, Arc::downgrade(&snapshot));
        Ok(FixtureMongoPage {
            page: QueryPage {
                operation_id: OperationId(page_id),
                page_id,
                columns,
                row_count: (end - start) as u64,
                total_count: Some(total_count),
                total_is_exact: true,
                flags: engine_driver_api::PageFlags {
                    truncated: false,
                    cancelled: false,
                    read_only: false,
                },
            },
            lease,
        })
    }

    pub fn inspect_mongo_row(&self, page_id: u64, row_index: u64) -> Result<RowFields, String> {
        let document = self
            .mongo_pages
            .get(&page_id)
            .and_then(|page| page.upgrade())
            .and_then(|page| {
                page.documents
                    .get(usize::try_from(row_index).ok()?)
                    .cloned()
            })
            .ok_or_else(|| "fixture Mongo page or row is no longer available".to_string())?;
        let identity = document
            .get("_id")
            .map(mongo_extended_json)
            .map(|id| serde_json::json!({"_id": id}))
            .unwrap_or(serde_json::Value::Null);
        Ok(RowFields {
            page_id,
            row_index,
            fields: document
                .iter()
                .map(|(name, value)| RowField {
                    name: name.into(),
                    native_type: mongo_native_type(value).into(),
                    display_text: mongo_display_text(value),
                    value: value.clone().into_canonical_extjson(),
                    editable: !identity.is_null() && name != "_id",
                })
                .collect(),
            mutation_identity: identity,
        })
    }

    pub fn apply_mongo_mutations(
        &mut self,
        connection: &str,
        database: &str,
        collection: &str,
        requests: &[MutationRequest],
    ) -> Result<usize, String> {
        if self.connection_kind(connection) != Some(ConnectionKind::Mongodb) {
            return Err("Mongo fixture connection is required".into());
        }
        let identity = FixtureIdentity {
            connection: connection.into(),
            database: database.into(),
            schema: String::new(),
            object: collection.into(),
        };
        let mut rows = self
            .mongo_rows_by_identity
            .get(&identity)
            .cloned()
            .unwrap_or_else(|| self.mongo_seed_rows.clone());
        let mut next_document_id = self.next_mongo_document_id;
        for request in requests {
            if request.database.as_deref() != Some(database)
                || request.object.as_deref() != Some(collection)
            {
                return Err("fixture mutation context does not match active collection".into());
            }
            match request.kind.as_str() {
                "update" => apply_mongo_update(&mut rows, request)?,
                "delete" => apply_mongo_delete(&mut rows, request)?,
                "insert" => apply_mongo_insert(&mut rows, request, &mut next_document_id)?,
                kind => return Err(format!("unsupported fixture Mongo mutation '{kind}'")),
            }
        }
        if !requests.is_empty() {
            self.mongo_rows_by_identity.insert(identity, rows);
            self.next_mongo_document_id = next_document_id;
        }
        Ok(requests.len())
    }

    fn connection_kind(&self, id: &str) -> Option<ConnectionKind> {
        self.connections
            .iter()
            .find(|connection| connection.matches_id(id))
            .map(|connection| connection.kind)
    }
}

/// `(name, argument types, table)` of the sample schema objects of a kind.
fn schema_object_names(
    kind: navigator::SchemaObjectKind,
) -> &'static [(&'static str, Option<&'static str>, Option<&'static str>)] {
    use navigator::SchemaObjectKind::*;
    match kind {
        Function => &[
            ("add_tax", Some("numeric, numeric"), None),
            ("order_total", Some("bigint"), None),
            ("order_total", Some("bigint, date"), None),
            ("touch_updated_at", Some(""), None),
        ],
        Procedure => &[("archive_orders", Some("date"), None)],
        Sequence => &[("orders_id_seq", None, None), ("invoice_no_seq", None, None)],
        Trigger => &[
            ("orders_touch", None, Some("orders")),
            ("orders_audit", None, Some("orders")),
        ],
        Type => &[("order_status", None, None), ("money_pair", None, None), ("positive_int", None, None)],
        Event => &[("nightly_cleanup", None, None)],
        MaterializedView => &[],
    }
}

/// The label the sidebar shows for a sample object (what the drivers print).
fn schema_object_label(reference: &navigator::SchemaObjectRef) -> String {
    match (&reference.arguments, &reference.table) {
        (Some(arguments), _) => format!("{}({arguments})", reference.name),
        (None, Some(table)) => format!("{} on {table}", reference.name),
        (None, None) => reference.name.clone(),
    }
}

/// The source the fixture engine answers for a sample schema object (the fixture
/// stand-in for `describe_object` with a `SchemaObjectRef`).
pub fn schema_object_source(
    fixture: FixtureKind,
    reference: &navigator::SchemaObjectRef,
) -> Result<String, String> {
    use navigator::SchemaObjectKind::*;
    if fixture == FixtureKind::Error {
        return Err("fixture catalog error".into());
    }
    let schema = reference.schema.as_deref().unwrap_or("public");
    let name = &reference.name;
    let arguments = reference.arguments.as_deref().unwrap_or_default();
    Ok(match reference.kind {
        Function if name == "touch_updated_at" => format!(
            "CREATE OR REPLACE FUNCTION {schema}.{name}()\n RETURNS trigger\n LANGUAGE plpgsql\nAS $function$\nBEGIN\n    NEW.updated_at := now();\n    RETURN NEW;\nEND;\n$function$;"
        ),
        Function => format!(
            "CREATE OR REPLACE FUNCTION {schema}.{name}({})\n RETURNS numeric\n LANGUAGE sql\n IMMUTABLE\nAS $function$\n    -- {name}({arguments})\n    SELECT $1 * 1.2\n$function$;",
            arguments
                .split(',')
                .enumerate()
                .map(|(index, ty)| format!("p{} {}", index + 1, ty.trim()))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Procedure => format!(
            "CREATE OR REPLACE PROCEDURE {schema}.{name}(cutoff {arguments})\n LANGUAGE sql\nAS $procedure$\n    DELETE FROM {schema}.orders WHERE created < cutoff;\n$procedure$;"
        ),
        Sequence => format!(
            "CREATE SEQUENCE {schema}.{name} INCREMENT BY 1 MINVALUE 1 MAXVALUE 9223372036854775807 START WITH 1 CACHE 1 NO CYCLE;"
        ),
        Trigger => format!(
            "CREATE TRIGGER {name} BEFORE UPDATE ON {schema}.{} FOR EACH ROW EXECUTE FUNCTION {schema}.touch_updated_at();",
            reference.table.as_deref().unwrap_or("orders")
        ),
        Type if name == "order_status" => {
            format!("CREATE TYPE {schema}.{name} AS ENUM ('pending', 'paid', 'shipped', 'refunded');")
        }
        Type if name == "money_pair" => {
            format!("CREATE TYPE {schema}.{name} AS (\n    amount numeric(12,2),\n    currency character(3)\n);")
        }
        Type => format!(
            "CREATE DOMAIN {schema}.{name} AS integer DEFAULT 1 NOT NULL\n    CONSTRAINT {name}_check CHECK ((VALUE > 0));"
        ),
        Event => format!(
            "CREATE EVENT {name} ON SCHEDULE EVERY 1 DAY DO DELETE FROM orders WHERE created < NOW() - INTERVAL 90 DAY;"
        ),
        MaterializedView => format!(
            "CREATE MATERIALIZED VIEW {schema}.{name} AS\nSELECT date_trunc('month', created) AS month, sum(total) AS revenue\nFROM {schema}.orders\nGROUP BY 1\nWITH DATA;"
        ),
    })
}

fn connection(id: &str, name: &str, kind: ConnectionKind, uri: &str) -> SavedConnection {
    SavedConnection {
        id: Uuid::parse_str(id).expect("fixture IDs are valid UUIDs"),
        name: name.into(),
        uri: uri.into(),
        kind,
        save_password: false,
        is_pinned: kind == ConnectionKind::Postgres,
        disabled: false,
        ssh_tunnel: false,
    }
}

impl FixtureRow {
    fn new(index: usize) -> Self {
        Self {
            id: (index + 1) as i64,
            name: match index {
                0 => "東京 café".into(),
                1 => "A very long fixture label that exercises clipping without losing its full value".into(),
                _ => format!("customer-{index:03}"),
            },
            status: (!index.is_multiple_of(11)).then(|| {
                if index.is_multiple_of(2) {
                    "active".into()
                } else {
                    "pending".into()
                }
            }),
            active: index.is_multiple_of(2),
            created: 20_000 + index as i32,
            score: index as f64 / 10.0,
            big_number: 9_000_000_000_000_000_000_u64.saturating_add(index as u64),
            payload: format!(r##"{{"tags":["fixture", "row-{index}"], "nested":{{"ok":true}}}}"##),
        }
    }
}

fn record_batch(rows: &[FixtureRow]) -> Result<RecordBatch, String> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("name", DataType::Utf8, false),
        Field::new("status", DataType::Utf8, true),
        Field::new("active", DataType::Boolean, false),
        Field::new("created", DataType::Date32, false),
        Field::new("score", DataType::Float64, false),
        Field::new("big_number", DataType::UInt64, false),
        Field::new("payload", DataType::Utf8, false),
    ]));
    let arrays: Vec<ArrayRef> = vec![
        Arc::new(Int64Array::from_iter_values(rows.iter().map(|row| row.id))),
        Arc::new(StringArray::from_iter_values(
            rows.iter().map(|row| row.name.as_str()),
        )),
        Arc::new(StringArray::from(
            rows.iter()
                .map(|row| row.status.as_deref())
                .collect::<Vec<_>>(),
        )),
        Arc::new(BooleanArray::from(
            rows.iter().map(|row| row.active).collect::<Vec<_>>(),
        )),
        Arc::new(Date32Array::from_iter_values(
            rows.iter().map(|row| row.created),
        )),
        Arc::new(Float64Array::from_iter_values(
            rows.iter().map(|row| row.score),
        )),
        Arc::new(UInt64Array::from_iter_values(
            rows.iter().map(|row| row.big_number),
        )),
        Arc::new(StringArray::from_iter_values(
            rows.iter().map(|row| row.payload.as_str()),
        )),
    ];
    RecordBatch::try_new(schema, arrays).map_err(|error| error.to_string())
}

fn mutation_i64(value: &MutationValue) -> Option<i64> {
    match value {
        MutationValue::Number(value) | MutationValue::Text(value) => value.parse().ok(),
        _ => None,
    }
}

fn validate_update(update: &UpdateCellRequest, rows: &[FixtureRow]) -> Result<(), String> {
    if update.key_column != "id" {
        return Err("fixture updates require the id primary key".into());
    }
    let Some(id) = mutation_i64(&update.key_value) else {
        return Err("fixture row id must be an integer".into());
    };
    if !rows.iter().any(|row| row.id == id) {
        return Err(format!("fixture row id {id} was not found"));
    }
    match (update.column.as_str(), &update.value) {
        ("name", MutationValue::Text(_))
        | ("status", MutationValue::Null | MutationValue::Text(_))
        | ("active", MutationValue::Boolean(_))
        | ("payload", MutationValue::Text(_)) => Ok(()),
        ("score", MutationValue::Number(value)) => value
            .parse::<f64>()
            .map(|_| ())
            .map_err(|_| "score must be a valid number".into()),
        ("big_number", MutationValue::Number(value)) => value
            .parse::<u64>()
            .map(|_| ())
            .map_err(|_| "big_number must be an unsigned integer".into()),
        ("id" | "created", _) => Err(format!("fixture column '{}' is read-only", update.column)),
        _ => Err(format!(
            "unsupported fixture update for '{}'",
            update.column
        )),
    }
}

fn apply_update(row: &mut FixtureRow, update: &UpdateCellRequest) {
    match (update.column.as_str(), &update.value) {
        ("name", MutationValue::Text(value)) => row.name = value.clone(),
        ("status", MutationValue::Null) => row.status = None,
        ("status", MutationValue::Text(value)) => row.status = Some(value.clone()),
        ("active", MutationValue::Boolean(value)) => row.active = *value,
        ("score", MutationValue::Number(value)) => {
            row.score = value.parse().expect("validated score")
        }
        ("big_number", MutationValue::Number(value)) => {
            row.big_number = value.parse().expect("validated big_number")
        }
        ("payload", MutationValue::Text(value)) => row.payload = value.clone(),
        _ => unreachable!("fixture update was validated"),
    }
}

#[derive(Debug, Clone, PartialEq)]
enum FixtureValue {
    Null,
    Int(i64),
    Float(f64),
    Text(String),
    Bool(bool),
}

impl FixtureValue {
    fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Int(value) => Some(*value as f64),
            Self::Float(value) => Some(*value),
            _ => None,
        }
    }

    /// Total order with NULL last (PostgreSQL ascending default).
    fn order(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        match (self, other) {
            (Self::Null, Self::Null) => Ordering::Equal,
            (Self::Null, _) => Ordering::Greater,
            (_, Self::Null) => Ordering::Less,
            (Self::Text(left), Self::Text(right)) => left.cmp(right),
            (Self::Bool(left), Self::Bool(right)) => left.cmp(right),
            (left, right) => match (left.as_f64(), right.as_f64()) {
                (Some(left), Some(right)) => left.total_cmp(&right),
                _ => Ordering::Equal,
            },
        }
    }
}

fn fixture_value(row: &FixtureRow, column: &str) -> Result<FixtureValue, String> {
    Ok(match column.to_ascii_lowercase().as_str() {
        "id" => FixtureValue::Int(row.id),
        "name" => FixtureValue::Text(row.name.clone()),
        "status" => row
            .status
            .clone()
            .map_or(FixtureValue::Null, FixtureValue::Text),
        "active" => FixtureValue::Bool(row.active),
        "created" => FixtureValue::Int(i64::from(row.created)),
        "score" => FixtureValue::Float(row.score),
        "big_number" => FixtureValue::Float(row.big_number as f64),
        "payload" => FixtureValue::Text(row.payload.clone()),
        _ => return Err(format!("column \"{column}\" does not exist")),
    })
}

fn fixture_unquote(name: &str) -> String {
    let name = name.trim();
    let name = name.rsplit('.').next().unwrap_or(name);
    name.trim_matches(|c| c == '"' || c == '`').to_string()
}

fn fixture_literal(text: &str) -> Result<FixtureValue, String> {
    let text = text.trim();
    if let Some(inner) = text.strip_prefix('\'').and_then(|t| t.strip_suffix('\'')) {
        return Ok(FixtureValue::Text(inner.replace("''", "'")));
    }
    match text.to_ascii_lowercase().as_str() {
        "true" => return Ok(FixtureValue::Bool(true)),
        "false" => return Ok(FixtureValue::Bool(false)),
        "null" => return Ok(FixtureValue::Null),
        _ => {}
    }
    if let Ok(value) = text.parse::<i64>() {
        return Ok(FixtureValue::Int(value));
    }
    text.parse::<f64>()
        .map(FixtureValue::Float)
        .map_err(|_| format!("syntax error at or near \"{text}\""))
}

/// Splits on `separator` (lowercase; matched case-insensitively) outside
/// single/double quotes.
fn fixture_split_outside_quotes(text: &str, separator: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let chars: Vec<char> = text.chars().collect();
    let sep: Vec<char> = separator.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        let ch = chars[index];
        match quote {
            Some(open) => {
                current.push(ch);
                if ch == open {
                    quote = None;
                }
                index += 1;
            }
            None if ch == '\'' || ch == '"' => {
                quote = Some(ch);
                current.push(ch);
                index += 1;
            }
            None if index + sep.len() <= chars.len()
                && chars[index..index + sep.len()]
                    .iter()
                    .zip(&sep)
                    .all(|(a, b)| a.to_ascii_lowercase() == *b) =>
            {
                parts.push(std::mem::take(&mut current));
                index += sep.len();
            }
            None => {
                current.push(ch);
                index += 1;
            }
        }
    }
    parts.push(current);
    parts
}

/// Parsed fixture WHERE expression (a SQL subset: comparisons, `IS [NOT] NULL`,
/// `[NOT] LIKE .. [ESCAPE ..]`, `[NOT] IN (..)`, `[NOT] BETWEEN .. AND ..`,
/// `NOT`, `AND`, `OR`, parentheses, `::text` casts). Evaluation uses SQL
/// three-valued logic (`None` = NULL / unknown); a row is kept only on `true`.
#[derive(Debug)]
enum FixtureExpr {
    Or(Vec<FixtureExpr>),
    And(Vec<FixtureExpr>),
    Not(Box<FixtureExpr>),
    Compare(FixtureOperand, String, FixtureOperand),
    IsNull(FixtureOperand, bool),
    Like {
        value: FixtureOperand,
        pattern: FixtureOperand,
        escape: Option<char>,
        negated: bool,
    },
    In {
        value: FixtureOperand,
        list: Vec<FixtureOperand>,
        negated: bool,
    },
    Between {
        value: FixtureOperand,
        low: FixtureOperand,
        high: FixtureOperand,
        negated: bool,
    },
}

#[derive(Debug)]
enum FixtureOperand {
    Column { name: String, as_text: bool },
    Literal(FixtureValue),
}

impl FixtureOperand {
    fn value(&self, row: &FixtureRow) -> Result<FixtureValue, String> {
        match self {
            Self::Literal(value) => Ok(value.clone()),
            Self::Column { name, as_text } => {
                let value = fixture_value(row, name)?;
                Ok(if *as_text { value.cast_text() } else { value })
            }
        }
    }
}

impl FixtureValue {
    fn cast_text(self) -> Self {
        match self {
            Self::Int(value) => Self::Text(value.to_string()),
            Self::Float(value) => Self::Text(value.to_string()),
            Self::Bool(value) => Self::Text(if value { "true" } else { "false" }.into()),
            other => other,
        }
    }

    /// Like PostgreSQL's unknown-literal coercion: a text literal compared with a
    /// number / boolean must parse as that type, otherwise it is an error.
    fn coerce_pair(left: Self, right: Self) -> Result<(Self, Self), String> {
        fn number(text: &str) -> Result<FixtureValue, String> {
            let text = text.trim();
            text.parse::<i64>()
                .map(FixtureValue::Int)
                .or_else(|_| text.parse::<f64>().map(FixtureValue::Float))
                .map_err(|_| format!("invalid input syntax for type numeric: \"{text}\""))
        }
        fn boolean(text: &str) -> Result<FixtureValue, String> {
            match text.trim().to_ascii_lowercase().as_str() {
                "true" | "t" | "1" | "yes" => Ok(FixtureValue::Bool(true)),
                "false" | "f" | "0" | "no" => Ok(FixtureValue::Bool(false)),
                _ => Err(format!("invalid input syntax for type boolean: \"{text}\"")),
            }
        }
        Ok(match (left, right) {
            (left @ (Self::Int(_) | Self::Float(_)), Self::Text(text)) => (left, number(&text)?),
            (Self::Text(text), right @ (Self::Int(_) | Self::Float(_))) => (number(&text)?, right),
            (left @ Self::Bool(_), Self::Text(text)) => (left, boolean(&text)?),
            (Self::Text(text), right @ Self::Bool(_)) => (boolean(&text)?, right),
            pair => pair,
        })
    }
}

impl FixtureExpr {
    fn eval(&self, row: &FixtureRow) -> Result<Option<bool>, String> {
        Ok(match self {
            Self::Or(items) => {
                let mut unknown = false;
                for item in items {
                    match item.eval(row)? {
                        Some(true) => return Ok(Some(true)),
                        None => unknown = true,
                        Some(false) => {}
                    }
                }
                (!unknown).then_some(false)
            }
            Self::And(items) => {
                let mut unknown = false;
                for item in items {
                    match item.eval(row)? {
                        Some(false) => return Ok(Some(false)),
                        None => unknown = true,
                        Some(true) => {}
                    }
                }
                (!unknown).then_some(true)
            }
            Self::Not(item) => item.eval(row)?.map(|value| !value),
            Self::IsNull(operand, negated) => {
                Some((operand.value(row)? == FixtureValue::Null) != *negated)
            }
            Self::Compare(left, operator, right) => {
                let (left, right) = (left.value(row)?, right.value(row)?);
                if left == FixtureValue::Null || right == FixtureValue::Null {
                    return Ok(None);
                }
                let (left, right) = FixtureValue::coerce_pair(left, right)?;
                let ordering = left.order(&right);
                Some(match operator.as_str() {
                    "=" => ordering.is_eq(),
                    "<>" | "!=" => !ordering.is_eq(),
                    ">" => ordering.is_gt(),
                    ">=" => ordering.is_ge(),
                    "<" => ordering.is_lt(),
                    "<=" => ordering.is_le(),
                    other => return Err(format!("unsupported fixture operator {other}")),
                })
            }
            Self::Like {
                value,
                pattern,
                escape,
                negated,
            } => {
                let (FixtureValue::Text(text), FixtureValue::Text(pattern)) =
                    (value.value(row)?, pattern.value(row)?)
                else {
                    return Ok(None);
                };
                Some(fixture_like(&text, &pattern, *escape) != *negated)
            }
            Self::In {
                value,
                list,
                negated,
            } => {
                let value = value.value(row)?;
                if value == FixtureValue::Null {
                    return Ok(None);
                }
                let mut unknown = false;
                for item in list {
                    let item = item.value(row)?;
                    if item == FixtureValue::Null {
                        unknown = true;
                        continue;
                    }
                    let (left, right) = FixtureValue::coerce_pair(value.clone(), item)?;
                    if left.order(&right).is_eq() {
                        return Ok(Some(!*negated));
                    }
                }
                if unknown {
                    None
                } else {
                    Some(*negated)
                }
            }
            Self::Between {
                value,
                low,
                high,
                negated,
            } => {
                let (value, low, high) = (value.value(row)?, low.value(row)?, high.value(row)?);
                if [&value, &low, &high].contains(&&FixtureValue::Null) {
                    return Ok(None);
                }
                let (value_low, low) = FixtureValue::coerce_pair(value.clone(), low)?;
                let (value_high, high) = FixtureValue::coerce_pair(value, high)?;
                let inside = value_low.order(&low).is_ge() && value_high.order(&high).is_le();
                Some(inside != *negated)
            }
        })
    }
}

/// SQL LIKE (`%`, `_`, optional escape character) over chars.
fn fixture_like(text: &str, pattern: &str, escape: Option<char>) -> bool {
    // (char, is_wildcard): an escaped wildcard is a literal.
    let mut tokens: Vec<(char, bool)> = Vec::new();
    let mut chars = pattern.chars();
    while let Some(ch) = chars.next() {
        if Some(ch) == escape {
            if let Some(next) = chars.next() {
                tokens.push((next, false));
            }
        } else {
            tokens.push((ch, ch == '%' || ch == '_'));
        }
    }
    let text: Vec<char> = text.chars().collect();
    // reachable[j] = the first i tokens can match text[..j].
    let mut reachable = vec![false; text.len() + 1];
    reachable[0] = true;
    for (ch, wildcard) in tokens {
        let mut next = vec![false; text.len() + 1];
        if wildcard && ch == '%' {
            let mut any = false;
            for j in 0..=text.len() {
                any |= reachable[j];
                next[j] = any;
            }
        } else {
            for j in 0..text.len() {
                if reachable[j] && (wildcard || text[j] == ch) {
                    next[j + 1] = true;
                }
            }
        }
        reachable = next;
    }
    reachable[text.len()]
}

#[derive(Debug, Clone, PartialEq)]
enum FixtureToken {
    /// Bare word (keyword or unquoted identifier).
    Word(String),
    /// `"name"` / `` `name` `` (never a keyword).
    Quoted(String),
    Str(String),
    Num(String),
    Op(String),
    LParen,
    RParen,
    Comma,
    Dot,
    Cast,
}

fn fixture_tokenize(text: &str) -> Result<Vec<FixtureToken>, String> {
    let chars: Vec<char> = text.chars().collect();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        let ch = chars[index];
        match ch {
            c if c.is_whitespace() => index += 1,
            '\'' | '"' | '`' => {
                let mut value = String::new();
                index += 1;
                loop {
                    let Some(&next) = chars.get(index) else {
                        return Err(format!("unterminated quoted text at or near \"{text}\""));
                    };
                    index += 1;
                    if next == ch {
                        if chars.get(index) == Some(&ch) {
                            value.push(ch);
                            index += 1;
                            continue;
                        }
                        break;
                    }
                    value.push(next);
                }
                tokens.push(if ch == '\'' {
                    FixtureToken::Str(value)
                } else {
                    FixtureToken::Quoted(value)
                });
            }
            '(' => {
                tokens.push(FixtureToken::LParen);
                index += 1;
            }
            ')' => {
                tokens.push(FixtureToken::RParen);
                index += 1;
            }
            ',' => {
                tokens.push(FixtureToken::Comma);
                index += 1;
            }
            ':' if chars.get(index + 1) == Some(&':') => {
                tokens.push(FixtureToken::Cast);
                index += 2;
            }
            '=' | '<' | '>' | '!' => {
                let two: String = chars[index..(index + 2).min(chars.len())].iter().collect();
                let op = if matches!(two.as_str(), "<>" | "!=" | ">=" | "<=") {
                    two
                } else {
                    ch.to_string()
                };
                index += op.chars().count();
                tokens.push(FixtureToken::Op(op));
            }
            c if c.is_ascii_digit()
                || (matches!(c, '+' | '-' | '.')
                    && chars.get(index + 1).is_some_and(|n| n.is_ascii_digit() || *n == '.')) =>
            {
                let start = index;
                index += 1;
                while index < chars.len()
                    && (chars[index].is_ascii_digit()
                        || chars[index] == '.'
                        || (matches!(chars[index], 'e' | 'E')
                            && chars
                                .get(index + 1)
                                .is_some_and(|n| n.is_ascii_digit() || matches!(n, '+' | '-')))
                        || (matches!(chars[index], '+' | '-')
                            && matches!(chars[index - 1], 'e' | 'E')))
                {
                    index += 1;
                }
                tokens.push(FixtureToken::Num(chars[start..index].iter().collect()));
            }
            '.' => {
                tokens.push(FixtureToken::Dot);
                index += 1;
            }
            c if c.is_alphanumeric() || c == '_' => {
                let start = index;
                while index < chars.len() && (chars[index].is_alphanumeric() || chars[index] == '_')
                {
                    index += 1;
                }
                tokens.push(FixtureToken::Word(chars[start..index].iter().collect()));
            }
            other => return Err(format!("syntax error at or near \"{other}\"")),
        }
    }
    Ok(tokens)
}

struct FixtureParser {
    tokens: Vec<FixtureToken>,
    position: usize,
}

impl FixtureParser {
    fn peek(&self) -> Option<&FixtureToken> {
        self.tokens.get(self.position)
    }

    fn syntax<T>(&self) -> Result<T, String> {
        Err(match self.peek() {
            Some(token) => format!("syntax error at or near \"{}\"", fixture_token_text(token)),
            None => "syntax error at end of input".into(),
        })
    }

    fn word_is(&self, word: &str) -> bool {
        matches!(self.peek(), Some(FixtureToken::Word(w)) if w.eq_ignore_ascii_case(word))
    }

    fn eat_word(&mut self, word: &str) -> bool {
        let found = self.word_is(word);
        if found {
            self.position += 1;
        }
        found
    }

    fn eat(&mut self, token: &FixtureToken) -> bool {
        let found = self.peek() == Some(token);
        if found {
            self.position += 1;
        }
        found
    }

    fn expect(&mut self, token: &FixtureToken) -> Result<(), String> {
        if self.eat(token) {
            Ok(())
        } else {
            self.syntax()
        }
    }

    fn or(&mut self) -> Result<FixtureExpr, String> {
        let mut items = vec![self.and()?];
        while self.eat_word("or") {
            items.push(self.and()?);
        }
        Ok(if items.len() == 1 {
            items.remove(0)
        } else {
            FixtureExpr::Or(items)
        })
    }

    fn and(&mut self) -> Result<FixtureExpr, String> {
        let mut items = vec![self.not()?];
        while self.eat_word("and") {
            items.push(self.not()?);
        }
        Ok(if items.len() == 1 {
            items.remove(0)
        } else {
            FixtureExpr::And(items)
        })
    }

    fn not(&mut self) -> Result<FixtureExpr, String> {
        if self.eat_word("not") {
            return Ok(FixtureExpr::Not(Box::new(self.not()?)));
        }
        self.predicate()
    }

    fn predicate(&mut self) -> Result<FixtureExpr, String> {
        if self.eat(&FixtureToken::LParen) {
            let inner = self.or()?;
            self.expect(&FixtureToken::RParen)?;
            return Ok(inner);
        }
        let left = self.operand()?;
        if let Some(FixtureToken::Op(op)) = self.peek().cloned() {
            self.position += 1;
            let right = self.operand()?;
            return Ok(FixtureExpr::Compare(left, op, right));
        }
        if self.eat_word("is") {
            let negated = self.eat_word("not");
            if !self.eat_word("null") {
                return self.syntax();
            }
            return Ok(FixtureExpr::IsNull(left, negated));
        }
        let negated = self.eat_word("not");
        if self.eat_word("like") {
            let pattern = self.operand()?;
            let escape = if self.eat_word("escape") {
                match self.peek().cloned() {
                    Some(FixtureToken::Str(text)) if text.chars().count() == 1 => {
                        self.position += 1;
                        text.chars().next()
                    }
                    _ => return self.syntax(),
                }
            } else {
                None
            };
            return Ok(FixtureExpr::Like {
                value: left,
                pattern,
                escape,
                negated,
            });
        }
        if self.eat_word("in") {
            self.expect(&FixtureToken::LParen)?;
            let mut list = vec![self.operand()?];
            while self.eat(&FixtureToken::Comma) {
                list.push(self.operand()?);
            }
            self.expect(&FixtureToken::RParen)?;
            return Ok(FixtureExpr::In {
                value: left,
                list,
                negated,
            });
        }
        if self.eat_word("between") {
            let low = self.operand()?;
            if !self.eat_word("and") {
                return self.syntax();
            }
            let high = self.operand()?;
            return Ok(FixtureExpr::Between {
                value: left,
                low,
                high,
                negated,
            });
        }
        self.syntax()
    }

    fn operand(&mut self) -> Result<FixtureOperand, String> {
        let Some(token) = self.peek().cloned() else {
            return self.syntax();
        };
        self.position += 1;
        let operand = match token {
            FixtureToken::Str(text) => FixtureOperand::Literal(FixtureValue::Text(text)),
            FixtureToken::Num(text) => FixtureOperand::Literal(fixture_literal(&text)?),
            FixtureToken::Word(word) => match word.to_ascii_lowercase().as_str() {
                "true" => FixtureOperand::Literal(FixtureValue::Bool(true)),
                "false" => FixtureOperand::Literal(FixtureValue::Bool(false)),
                "null" => FixtureOperand::Literal(FixtureValue::Null),
                _ => self.column(word)?,
            },
            FixtureToken::Quoted(name) => self.column(name)?,
            _ => {
                self.position -= 1;
                return self.syntax();
            }
        };
        Ok(operand)
    }

    /// `name`, `schema.name`, optionally followed by `::type`.
    fn column(&mut self, mut name: String) -> Result<FixtureOperand, String> {
        while self.eat(&FixtureToken::Dot) {
            match self.peek().cloned() {
                Some(FixtureToken::Word(next) | FixtureToken::Quoted(next)) => {
                    self.position += 1;
                    name = next;
                }
                _ => return self.syntax(),
            }
        }
        let mut as_text = false;
        while self.eat(&FixtureToken::Cast) {
            match self.peek().cloned() {
                Some(FixtureToken::Word(kind)) => {
                    self.position += 1;
                    as_text = kind.eq_ignore_ascii_case("text");
                }
                _ => return self.syntax(),
            }
        }
        Ok(FixtureOperand::Column { name, as_text })
    }
}

fn fixture_token_text(token: &FixtureToken) -> String {
    match token {
        FixtureToken::Word(text)
        | FixtureToken::Quoted(text)
        | FixtureToken::Str(text)
        | FixtureToken::Num(text)
        | FixtureToken::Op(text) => text.clone(),
        FixtureToken::LParen => "(".into(),
        FixtureToken::RParen => ")".into(),
        FixtureToken::Comma => ",".into(),
        FixtureToken::Dot => ".".into(),
        FixtureToken::Cast => "::".into(),
    }
}

fn fixture_parse_where(text: &str) -> Result<Option<FixtureExpr>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let mut parser = FixtureParser {
        tokens: fixture_tokenize(text)?,
        position: 0,
    };
    let expr = parser.or()?;
    if parser.position != parser.tokens.len() {
        return parser.syntax();
    }
    Ok(Some(expr))
}

fn fixture_parse_order(text: &str) -> Result<Vec<(String, bool)>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(Vec::new());
    }
    fixture_split_outside_quotes(text, ",")
        .into_iter()
        .map(|term| {
            let term = term.trim();
            let lower = term.to_ascii_lowercase();
            let (name, descending) = if lower.ends_with(" desc") {
                (&term[..term.len() - 5], true)
            } else if lower.ends_with(" asc") {
                (&term[..term.len() - 4], false)
            } else {
                (term, false)
            };
            let column = fixture_unquote(name);
            if column.is_empty() || column.contains(char::is_whitespace) {
                return Err(format!("syntax error at or near \"{term}\""));
            }
            Ok((column, descending))
        })
        .collect()
}

fn fixture_matches_filter(row: &FixtureRow, filter: &serde_json::Value) -> bool {
    let Some(filter) = filter.as_object() else {
        return false;
    };
    filter
        .iter()
        .all(|(column, expected)| match column.as_str() {
            "id" => expected.as_i64() == Some(row.id),
            "name" => expected.as_str() == Some(row.name.as_str()),
            "status" => expected
                .as_str()
                .map_or(expected.is_null() && row.status.is_none(), |value| {
                    row.status.as_deref() == Some(value)
                }),
            "active" => expected.as_bool() == Some(row.active),
            "score" => expected.as_f64() == Some(row.score),
            "big_number" => expected.as_u64() == Some(row.big_number),
            _ => false,
        })
}

fn fixture_sort_indices(
    indices: &mut [usize],
    rows: &[FixtureRow],
    sort: &serde_json::Value,
) -> Result<(), String> {
    let Some((column, direction)) = sort.as_object().and_then(|sort| sort.iter().next()) else {
        return Ok(());
    };
    let direction = direction
        .as_i64()
        .ok_or_else(|| "fixture sort direction must be numeric".to_string())?;
    let ordering = |left: usize, right: usize| {
        let left = &rows[left];
        let right = &rows[right];
        match column.as_str() {
            "id" => left.id.cmp(&right.id),
            "name" => left.name.cmp(&right.name),
            "status" => left.status.cmp(&right.status),
            "active" => left.active.cmp(&right.active),
            "created" => left.created.cmp(&right.created),
            "score" => left.score.total_cmp(&right.score),
            "big_number" => left.big_number.cmp(&right.big_number),
            _ => std::cmp::Ordering::Equal,
        }
    };
    indices.sort_unstable_by(|left, right| {
        let ordering = ordering(*left, *right);
        if direction < 0 {
            ordering.reverse()
        } else {
            ordering
        }
    });
    Ok(())
}

fn provider_page(
    catalog: &MongoFixtureCatalog,
    connection: &str,
    parent: Option<&str>,
    payload: Option<&serde_json::Value>,
    cursor: Option<&str>,
    limit: u64,
) -> Result<engine_driver_api::NavigatorPage, String> {
    let requested_limit = usize::try_from(limit.max(1)).unwrap_or(usize::MAX);
    let (items, parent_key) = match parent {
        None => {
            if payload.is_some() {
                return Err("malformed Mongo provider root request".into());
            }
            (
                catalog
                    .databases
                    .iter()
                    .map(|database| {
                        let database = database.clone();
                        provider_node(
                            format!("database:{database}"),
                            "database",
                            database.clone(),
                            true,
                            serde_json::json!({"database": database}),
                        )
                    })
                    .collect::<Vec<_>>(),
                "root".to_string(),
            )
        }
        Some(parent) => {
            let Some(database_id) = parent.strip_prefix("database:") else {
                return Err("Mongo provider parent is not a database".into());
            };
            let Some(database) = payload
                .and_then(|payload| payload.get("database"))
                .and_then(serde_json::Value::as_str)
            else {
                return Err("Mongo provider database payload is missing".into());
            };
            if database != database_id || !catalog.databases.iter().any(|name| name == database) {
                return Err("Mongo provider parent payload does not match".into());
            }
            (
                catalog
                    .collection_names(database)?
                    .iter()
                    .map(|collection| {
                        let collection = collection.clone();
                        provider_node(
                            format!("{parent}:collection:{collection}"),
                            "collection",
                            collection.clone(),
                            false,
                            serde_json::json!({
                                "database": database,
                                "collection": collection
                            }),
                        )
                    })
                    .collect::<Vec<_>>(),
                parent.to_string(),
            )
        }
    };
    let start = provider_cursor_offset(cursor, connection, &parent_key)?;
    if start > items.len() {
        return Err("Mongo provider cursor is out of range".into());
    }
    let server_page_limit = if parent.is_none() { 4 } else { 10 };
    let end = start
        .saturating_add(requested_limit.min(server_page_limit))
        .min(items.len());
    let next_cursor = (end < items.len()).then(|| format_cursor(connection, &parent_key, end));
    Ok(engine_driver_api::NavigatorPage {
        nodes: items[start..end].to_vec(),
        next_cursor,
    })
}

fn format_cursor(connection: &str, parent: &str, offset: usize) -> String {
    format!("fixture-v1:{connection}:{parent}:{offset}")
}

fn provider_cursor_offset(
    cursor: Option<&str>,
    connection: &str,
    parent: &str,
) -> Result<usize, String> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    let prefix = format!("fixture-v1:{connection}:{parent}:");
    let Some(offset) = cursor.strip_prefix(&prefix) else {
        return Err("malformed or wrong-parent Mongo provider cursor".into());
    };
    offset
        .parse::<usize>()
        .map_err(|_| "malformed Mongo provider cursor".into())
}

/// The fixture document that lacks the `nullable` field.
pub const MONGO_DOCUMENT_WITHOUT_NULLABLE: usize = 3;

fn mongo_document(index: usize) -> Document {
    let id = ObjectId::parse_str(format!("{:024x}", index + 1)).expect("fixture ObjectId");
    let created_at = DateTime::from_millis(1_725_148_800_000_i64 + index as i64 * 86_400_000);
    let balance = Decimal128::from_str(&format!("{}.25", index + 1)).expect("fixture decimal");
    let mut document = Document::new();
    document.insert("_id", Bson::ObjectId(id));
    document.insert("name", format!("mongo-customer-{index:03}"));
    // Every fifth document holds a real null; the fourth has no such field at
    // all (a missing field is neither a null nor the string "NULL").
    if index != MONGO_DOCUMENT_WITHOUT_NULLABLE {
        document.insert(
            "nullable",
            if index.is_multiple_of(5) {
                Bson::Null
            } else {
                Bson::String("present".into())
            },
        );
    }
    document.insert("active", index.is_multiple_of(2));
    document.insert("count", index as i64);
    document.insert("createdAt", created_at);
    document.insert("balance", Bson::Decimal128(balance));
    document.insert(
        "nested",
        bson::doc! {
            "ok": true,
            "index": index as i64,
            "updatedAt": created_at,
            "owner": { "name": format!("owner-{index:03}"), "tier": Bson::Int32(2) },
        },
    );
    document.insert(
        "items",
        Bson::Array(vec![
            Bson::Int32(index as i32),
            Bson::Boolean(true),
            Bson::Document(bson::doc! {
                "sku": format!("SKU-{index:03}"),
                "qty": Bson::Int64(3),
                "shippedAt": created_at,
            }),
            Bson::Document(bson::doc! {
                "sku": "SKU-XYZ",
                "qty": Bson::Int64(1),
                "tags": ["gift", "priority"],
            }),
        ]),
    );
    document.insert(
        "longText",
        format!("{} — nested BSON fixture {index}", "long text ".repeat(32)),
    );
    document
}

fn mongo_page_lease(
    column_source: &[Document],
    documents: Vec<Document>,
    page_id: u64,
) -> Result<(Vec<PageColumn>, PageLease, Arc<FixturePageSnapshot>), String> {
    let columns = mongo_columns(column_source);
    let mut data = Vec::new();
    let mut slices = Vec::with_capacity(documents.len());
    for document in &documents {
        let bytes = bson::to_vec(document).map_err(|error| error.to_string())?;
        let offset = u64::try_from(data.len()).map_err(|_| "fixture BSON page is too large")?;
        data.extend_from_slice(&bytes);
        slices.push(CPageSlice {
            offset,
            length: u64::try_from(bytes.len()).map_err(|_| "fixture BSON document is too large")?,
        });
    }
    let snapshot = Arc::new(FixturePageSnapshot {
        documents,
        data,
        slices,
    });
    let backing = Box::new(FixtureRawBacking {
        _snapshot: Arc::clone(&snapshot),
    });
    let data_ptr = if snapshot.data.is_empty() {
        std::ptr::null()
    } else {
        snapshot.data.as_ptr()
    };
    let data_len = snapshot.data.len() as u64;
    let slices_ptr = if snapshot.slices.is_empty() {
        std::ptr::null()
    } else {
        snapshot.slices.as_ptr()
    };
    let slice_count = snapshot.slices.len() as u64;
    let context = Box::into_raw(backing).cast::<c_void>();
    let lease = PageLease::from_raw(CPageLease {
        data: data_ptr,
        data_len,
        generation: page_id,
        release: Some(release_fixture_page),
        release_context: context,
        slices: slices_ptr,
        slice_count,
    })
    .map_err(|error| error.to_string())?;
    Ok((columns, lease, snapshot))
}

struct FixtureRawBacking {
    _snapshot: Arc<FixturePageSnapshot>,
}

struct FixturePageSnapshot {
    documents: Vec<Document>,
    data: Vec<u8>,
    slices: Vec<CPageSlice>,
}

unsafe extern "C" fn release_fixture_page(context: *mut c_void) {
    if !context.is_null() {
        // SAFETY: the context is transferred to PageLease and released once by its final owner.
        unsafe { drop(Box::from_raw(context.cast::<FixtureRawBacking>())) };
    }
}

fn mongo_columns(documents: &[Document]) -> Vec<PageColumn> {
    let names = if documents.is_empty() {
        vec![
            "_id",
            "name",
            "nullable",
            "active",
            "count",
            "createdAt",
            "balance",
            "nested",
            "items",
            "longText",
        ]
    } else {
        let mut names = Vec::new();
        for document in documents {
            for (name, _) in document {
                if !names.iter().any(|existing| existing == name) {
                    names.push(name.as_str());
                }
            }
        }
        names
    };
    names
        .into_iter()
        .map(|name| PageColumn {
            id: name.into(),
            name: name.into(),
            data_type: documents
                .iter()
                .find_map(|document| {
                    document
                        .get(name)
                        .filter(|value| !matches!(value, Bson::Null))
                })
                .map(mongo_native_type)
                .unwrap_or_else(|| mongo_default_native_type(name))
                .into(),
            nullable: name != "_id",
        })
        .collect()
}

fn mongo_default_native_type(name: &str) -> &'static str {
    match name {
        "_id" => "object_id",
        "name" | "nullable" | "longText" => "string",
        "active" => "boolean",
        "count" => "int64",
        "createdAt" => "date_time",
        "balance" => "decimal128",
        "nested" => "document",
        "items" => "array",
        _ => "null",
    }
}

fn mongo_native_type(value: &Bson) -> &'static str {
    crate::bson_page::native_type(value)
}

/// Canonical Extended JSON, the form the driver sends (lossless for every type).
fn mongo_extended_json(value: &Bson) -> serde_json::Value {
    value.clone().into_canonical_extjson()
}

fn mongo_display_text(value: &Bson) -> String {
    match value {
        Bson::String(value) => value.clone(),
        Bson::Boolean(value) => value.to_string(),
        Bson::Int32(value) => value.to_string(),
        Bson::Int64(value) => value.to_string(),
        Bson::Double(value) => value.to_string(),
        Bson::ObjectId(value) => format!("ObjectId({})", value.to_hex()),
        Bson::DateTime(value) => value.to_string(),
        Bson::Decimal128(value) => value.to_string(),
        Bson::Null => "NULL".into(),
        value => serde_json::to_string(&mongo_extended_json(value)).unwrap_or_default(),
    }
}

fn document_identity(document: &Document) -> serde_json::Value {
    document
        .get("_id")
        .map(mongo_extended_json)
        .map(|id| serde_json::json!({"_id": id}))
        .unwrap_or_else(|| serde_json::json!({}))
}

fn apply_mongo_update(rows: &mut [Document], request: &MutationRequest) -> Result<(), String> {
    let index = find_mongo_identity(rows, &request.identity)?;
    let operator = |name: &str| request.payload.get(name).and_then(serde_json::Value::as_object);
    let (sets, unsets) = (operator("$set"), operator("$unset"));
    if sets.is_none() && unsets.is_none() {
        return Err("Mongo fixture update requires a $set or $unset object".into());
    }
    if [sets, unsets].into_iter().flatten().any(|fields| fields.contains_key("_id")) {
        return Err("Mongo fixture _id is read-only".into());
    }
    for (field, value) in sets.into_iter().flatten() {
        rows[index].insert(field, json_to_bson(value)?);
    }
    for field in unsets.into_iter().flatten().map(|(field, _)| field) {
        rows[index].remove(field);
    }
    Ok(())
}

fn apply_mongo_delete(rows: &mut Vec<Document>, request: &MutationRequest) -> Result<(), String> {
    let index = find_mongo_identity(rows, &request.identity)?;
    rows.remove(index);
    Ok(())
}

fn apply_mongo_insert(
    rows: &mut Vec<Document>,
    request: &MutationRequest,
    next_id: &mut u64,
) -> Result<(), String> {
    let Bson::Document(mut document) = json_to_bson(&request.payload)? else {
        return Err("Mongo fixture insert requires a JSON object".into());
    };
    if !document.contains_key("_id") {
        let id = next_unique_object_id(rows, next_id)?;
        document.insert("_id", id);
    }
    let identity = document_identity(&document);
    if rows.iter().any(|row| document_identity(row) == identity) {
        return Err("Mongo fixture insert rejected: duplicate_id".into());
    }
    if let Some(Bson::ObjectId(id)) = document.get("_id") {
        if let Ok(value) = u64::from_str_radix(&id.to_hex(), 16) {
            *next_id = (*next_id).max(value.saturating_add(1));
        }
    }
    rows.push(document);
    Ok(())
}

fn next_unique_object_id(rows: &[Document], next_id: &mut u64) -> Result<ObjectId, String> {
    loop {
        let value = *next_id;
        let id = ObjectId::parse_str(format!("{value:024x}"))
            .map_err(|_| "Mongo fixture ObjectId allocator exhausted".to_string())?;
        *next_id = value
            .checked_add(1)
            .ok_or_else(|| "Mongo fixture ObjectId allocator exhausted".to_string())?;
        if !rows
            .iter()
            .any(|row| row.get_object_id("_id").ok() == Some(id))
        {
            return Ok(id);
        }
    }
}

fn find_mongo_identity(rows: &[Document], identity: &serde_json::Value) -> Result<usize, String> {
    let Some(index) = rows
        .iter()
        .position(|row| document_identity(row) == *identity)
    else {
        return Err("Mongo fixture mutation rejected: stale or mismatched identity".into());
    };
    Ok(index)
}

fn json_to_bson(value: &serde_json::Value) -> Result<Bson, String> {
    Bson::try_from(value.clone()).map_err(|error| error.to_string())
}

fn mongo_matches_filter(document: &Document, filter: &serde_json::Value) -> bool {
    let Some(fields) = filter.as_object() else {
        return false;
    };
    fields.iter().all(|(field, expected)| {
        if field == "$and" {
            return expected.as_array().is_some_and(|values| {
                values
                    .iter()
                    .all(|value| mongo_matches_filter(document, value))
            });
        }
        if field == "$or" {
            return expected.as_array().is_some_and(|values| {
                values
                    .iter()
                    .any(|value| mongo_matches_filter(document, value))
            });
        }
        let Some(actual) = document.get(field).map(mongo_extended_json) else {
            return expected.is_null();
        };
        if let Some(operators) = expected.as_object() {
            return operators
                .iter()
                .all(|(operator, value)| match operator.as_str() {
                    "$ne" => actual != *value,
                    "$gt" => {
                        mongo_ordering(&actual, value).is_some_and(|ordering| ordering.is_gt())
                    }
                    "$gte" => {
                        mongo_ordering(&actual, value).is_some_and(|ordering| !ordering.is_lt())
                    }
                    "$lt" => {
                        mongo_ordering(&actual, value).is_some_and(|ordering| ordering.is_lt())
                    }
                    "$lte" => {
                        mongo_ordering(&actual, value).is_some_and(|ordering| !ordering.is_gt())
                    }
                    _ => false,
                });
        }
        actual == *expected
    })
}

fn mongo_ordering(
    left: &serde_json::Value,
    right: &serde_json::Value,
) -> Option<std::cmp::Ordering> {
    match (left.as_f64(), right.as_f64()) {
        (Some(left), Some(right)) => left.partial_cmp(&right),
        _ => left.to_string().partial_cmp(&right.to_string()),
    }
}

fn mongo_sort_indices(
    indices: &mut [usize],
    rows: &[Document],
    sort: &serde_json::Value,
) -> Result<(), String> {
    let Some((field, direction)) = sort.as_object().and_then(|sort| sort.iter().next()) else {
        return Ok(());
    };
    let direction = direction
        .as_i64()
        .ok_or_else(|| "fixture sort direction must be numeric".to_string())?;
    indices.sort_unstable_by(|left, right| {
        let left = rows[*left]
            .get(field)
            .map(mongo_extended_json)
            .unwrap_or(serde_json::Value::Null);
        let right = rows[*right]
            .get(field)
            .map(mongo_extended_json)
            .unwrap_or(serde_json::Value::Null);
        let ordering = mongo_ordering(&left, &right).unwrap_or(std::cmp::Ordering::Equal);
        if direction < 0 {
            ordering.reverse()
        } else {
            ordering
        }
    });
    Ok(())
}

fn field_ref(value: &Bson) -> Option<&str> {
    match value {
        Bson::String(text) => text.strip_prefix('$'),
        _ => None,
    }
}

fn bson_number(value: &Bson) -> Option<f64> {
    match value {
        Bson::Int32(number) => Some(f64::from(*number)),
        Bson::Int64(number) => Some(*number as f64),
        Bson::Double(number) => Some(*number),
        _ => None,
    }
}

fn bson_count(value: &Bson) -> Option<usize> {
    bson_number(value).filter(|number| *number >= 0.0).map(|number| number as usize)
}

fn bson_compare(left: Option<&Bson>, right: Option<&Bson>) -> std::cmp::Ordering {
    let json = |value: Option<&Bson>| value.map(mongo_extended_json).unwrap_or(serde_json::Value::Null);
    mongo_ordering(&json(left), &json(right)).unwrap_or(std::cmp::Ordering::Equal)
}

/// Minimal in-memory pipeline evaluator for the scenario fixture:
/// `$match` (top-level fields), `$sort`, `$skip`, `$limit`, `$project`,
/// `$group` (`$sum` / `$min` / `$max` / `$avg`), `$count`, `$unwind` and
/// `$out` / `$merge` (which produce no documents). Anything else is an
/// error rather than a silent wrong answer.
fn fixture_run_pipeline(mut rows: Vec<Document>, pipeline: &[Document]) -> Result<Vec<Document>, String> {
    for stage in pipeline {
        let (operator, body) = stage
            .iter()
            .next()
            .ok_or("fixture aggregate: empty stage")?;
        match (operator.as_str(), body) {
            ("$match", Bson::Document(filter)) => {
                let filter = Bson::Document(filter.clone()).into_relaxed_extjson();
                rows.retain(|row| mongo_matches_filter(row, &filter));
            }
            ("$sort", Bson::Document(keys)) => {
                rows.sort_by(|left, right| {
                    for (field, direction) in keys {
                        let ordering = bson_compare(left.get(field), right.get(field));
                        let ordering = if bson_number(direction).is_some_and(|number| number < 0.0) {
                            ordering.reverse()
                        } else {
                            ordering
                        };
                        if ordering != std::cmp::Ordering::Equal {
                            return ordering;
                        }
                    }
                    std::cmp::Ordering::Equal
                });
            }
            ("$skip", count) => {
                let count = bson_count(count).ok_or("fixture aggregate: $skip needs a number")?;
                rows.drain(..count.min(rows.len()));
            }
            ("$limit", count) => {
                let count = bson_count(count).ok_or("fixture aggregate: $limit needs a number")?;
                rows.truncate(count);
            }
            ("$project", Bson::Document(spec)) => {
                let include = |value: &Bson| !matches!(value, Bson::Int32(0) | Bson::Int64(0) | Bson::Boolean(false));
                let inclusion = spec.iter().any(|(field, value)| field != "_id" && include(value));
                rows = rows
                    .into_iter()
                    .map(|row| {
                        let mut out = Document::new();
                        if inclusion {
                            if spec.get("_id").is_none_or(include) {
                                if let Some(id) = row.get("_id") {
                                    out.insert("_id", id.clone());
                                }
                            }
                            for (field, value) in spec {
                                if field != "_id" && include(value) {
                                    if let Some(found) = row.get(field) {
                                        out.insert(field.clone(), found.clone());
                                    }
                                }
                            }
                        } else {
                            for (field, value) in &row {
                                if spec.get(field).is_none_or(include) {
                                    out.insert(field.clone(), value.clone());
                                }
                            }
                        }
                        out
                    })
                    .collect();
            }
            ("$count", Bson::String(name)) => {
                rows = vec![bson::doc! { name.as_str(): rows.len() as i64 }];
            }
            ("$unwind", path) => {
                let field = field_ref(path).ok_or("fixture aggregate: $unwind needs \"$field\"")?;
                rows = rows
                    .into_iter()
                    .flat_map(|row| match row.get(field) {
                        Some(Bson::Array(items)) => items
                            .iter()
                            .map(|item| {
                                let mut copy = row.clone();
                                copy.insert(field, item.clone());
                                copy
                            })
                            .collect::<Vec<_>>(),
                        _ => Vec::new(),
                    })
                    .collect();
            }
            ("$group", Bson::Document(spec)) => {
                let key_of = |row: &Document| match spec.get("_id") {
                    Some(value) => match field_ref(value) {
                        Some(field) => row.get(field).cloned().unwrap_or(Bson::Null),
                        None => value.clone(),
                    },
                    None => Bson::Null,
                };
                let mut order: Vec<(Bson, Vec<Document>)> = Vec::new();
                for row in rows {
                    let key = key_of(&row);
                    match order.iter_mut().find(|(existing, _)| *existing == key) {
                        Some((_, members)) => members.push(row),
                        None => order.push((key, vec![row])),
                    }
                }
                rows = order
                    .into_iter()
                    .map(|(key, members)| {
                        let mut out = bson::doc! { "_id": key };
                        for (name, accumulator) in spec.iter().filter(|(name, _)| *name != "_id") {
                            let Bson::Document(accumulator) = accumulator else { continue };
                            let Some((function, argument)) = accumulator.iter().next() else { continue };
                            let values: Vec<f64> = members
                                .iter()
                                .filter_map(|member| match field_ref(argument) {
                                    Some(field) => member.get(field).and_then(bson_number),
                                    None => bson_number(argument),
                                })
                                .collect();
                            let value = match function.as_str() {
                                "$sum" => Bson::Double(values.iter().sum()),
                                "$min" => values.iter().copied().reduce(f64::min).map_or(Bson::Null, Bson::Double),
                                "$max" => values.iter().copied().reduce(f64::max).map_or(Bson::Null, Bson::Double),
                                "$avg" if !values.is_empty() => {
                                    Bson::Double(values.iter().sum::<f64>() / values.len() as f64)
                                }
                                _ => Bson::Null,
                            };
                            let value = match value {
                                Bson::Double(number) if number.fract() == 0.0 && number.abs() < 1e15 => {
                                    Bson::Int64(number as i64)
                                }
                                other => other,
                            };
                            out.insert(name.clone(), value);
                        }
                        out
                    })
                    .collect();
            }
            ("$out" | "$merge", _) => rows.clear(),
            (other, _) => return Err(format!("fixture aggregate does not support {other}")),
        }
    }
    Ok(rows)
}

fn provider_node(
    id: String,
    kind: &str,
    label: impl Into<String>,
    has_children: bool,
    payload: serde_json::Value,
) -> engine_driver_api::NavigatorNode {
    engine_driver_api::NavigatorNode {
        id,
        kind: kind.into(),
        label: label.into(),
        subtitle: Some("fixture".into()),
        icon: None,
        has_children,
        payload,
        actions: vec!["open".into(), "refresh".into()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clickhouse_capture_fixture_adds_a_schemaless_sql_connection() {
        let fixture = UiFixture::with_clickhouse(FixtureKind::Sample, true);
        let connections = fixture.connections();
        assert_eq!(connections.len(), 4);
        assert_eq!(connections[3].name, "ClickHouse fixture");
        assert_eq!(connections[3].kind, ConnectionKind::Clickhouse);
        assert!(connections[3].matches_id(CLICKHOUSE_ID));

        let children = |load| match fixture.load(&load).expect("fixture load") {
            FixtureLoad::Children(children) => children,
            _ => panic!("children expected"),
        };
        let databases = children(LoadRequest::Databases { connection: CLICKHOUSE_ID.into() });
        assert_eq!(databases.len(), 2);
        // No schema level: one schema named like its database.
        let schemas = children(LoadRequest::Schemas {
            connection: CLICKHOUSE_ID.into(),
            database: "analytics".into(),
        });
        assert_eq!(schemas.len(), 1);
        assert_eq!(schemas[0].label, "analytics");

        let properties = crate::ui_fixture_properties::table_properties(
            FixtureKind::Sample,
            CLICKHOUSE_ID,
            "analytics",
            "events",
        )
        .expect("ClickHouse properties");
        let keys = properties.keys.expect("keys");
        for name in ["PRIMARY", "ORDER BY", "PARTITION BY", "TTL"] {
            assert!(keys.iter().any(|key| key.name == name), "{name}");
        }
        assert!(properties.ddl.unwrap().starts_with("-- engine: MergeTree"));
        // Off by default.
        assert_eq!(UiFixture::with_clickhouse(FixtureKind::Sample, false).connections().len(), 3);
    }

    #[test]
    fn sample_fixture_has_stable_connections_and_typed_pages() {
        let fixture = UiFixture::new(FixtureKind::Sample);
        let connections = fixture.connections();

        assert_eq!(connections.len(), 3);
        assert_eq!(connections[0].name, "Mongo fixture");
        assert_eq!(connections[1].name, "Postgres fixture");
        assert_eq!(connections[2].name, "SQLite fixture");

        let page = fixture
            .query_page(POSTGRES_ID, "warehouse", "public", "orders", 0, 50)
            .expect("fixture page");
        assert_eq!(page.total_count, 240);
        assert_eq!(page.batches[0].num_rows(), 50);
        assert_eq!(page.batches[0].schema_ref().field(0).name(), "id");
        assert_eq!(page.batches[0].schema_ref().field(3).name(), "active");
        let last_page = fixture
            .query_page(POSTGRES_ID, "warehouse", "public", "orders", 200, 50)
            .expect("last fixture page");
        assert_eq!(last_page.total_count, 240);
        assert_eq!(last_page.batches[0].num_rows(), 40);
    }

    #[test]
    fn fixture_pages_are_isolated_and_deterministic() {
        let first = UiFixture::new(FixtureKind::Sample)
            .query_page(POSTGRES_ID, "warehouse", "public", "orders", 50, 50)
            .expect("second page");
        let second = UiFixture::new(FixtureKind::Sample)
            .query_page(POSTGRES_ID, "warehouse", "public", "orders", 50, 50)
            .expect("second page");

        assert_eq!(first.batches[0], second.batches[0]);
        assert_ne!(
            first.batches[0],
            UiFixture::new(FixtureKind::Sample)
                .query_page(POSTGRES_ID, "warehouse", "public", "orders", 0, 50)
                .expect("first page")
                .batches[0]
        );
    }

    #[test]
    fn fixture_pages_follow_the_result_footer_offsets() {
        let fixture = UiFixture::new(FixtureKind::Sample);
        for (offset, expected_id) in [(0, 1), (50, 51), (100, 101), (150, 151), (200, 201)] {
            let page = fixture
                .query_page(POSTGRES_ID, "warehouse", "public", "orders", offset, 50)
                .expect("fixture page");
            let ids = page.batches[0]
                .column(0)
                .as_any()
                .downcast_ref::<Int64Array>()
                .unwrap();
            assert_eq!(ids.value(0), expected_id);
            assert_eq!(page.total_count, 240);
        }
    }

    #[test]
    fn empty_fixture_retains_column_schema() {
        let page = UiFixture::new(FixtureKind::Empty)
            .query_page(POSTGRES_ID, "warehouse", "public", "orders", 0, 50)
            .expect("empty fixture");
        assert_eq!(page.total_count, 0);
        assert_eq!(page.batches.len(), 1);
        assert_eq!(page.batches[0].num_rows(), 0);
        assert_eq!(page.batches[0].num_columns(), 8);
    }

    #[test]
    fn fixture_sql_update_changes_only_the_target_row() {
        let mut fixture = UiFixture::new(FixtureKind::Sample);
        let updated = fixture.apply_sql_updates(
            POSTGRES_ID,
            "warehouse",
            "public",
            "orders",
            &[UpdateCellRequest {
                database: "warehouse".into(),
                schema: "public".into(),
                table: "orders".into(),
                key_column: "id".into(),
                key_value: MutationValue::Number("2".into()),
                column: "name".into(),
                value: MutationValue::Text("edited fixture".into()),
            }],
        );

        assert_eq!(updated.expect("fixture update"), 1);
        let page = fixture
            .query_page(POSTGRES_ID, "warehouse", "public", "orders", 0, 3)
            .expect("fixture page");
        assert_eq!(
            page.batches[0]
                .column(1)
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap()
                .value(1),
            "edited fixture"
        );
        assert_eq!(
            page.batches[0]
                .column(1)
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap()
                .value(0),
            "東京 café"
        );
    }

    /// Ids of the `orders` fixture rows kept by a WHERE fragment.
    fn orders_where_ids(where_clause: &str) -> Result<Vec<i64>, String> {
        let fixture = UiFixture::new(FixtureKind::Sample);
        let page = fixture.query_sql_table_page(
            (POSTGRES_ID, "warehouse", "public", "orders"),
            0,
            1000,
            (where_clause, ""),
        )?;
        Ok(page
            .batches
            .first()
            .map(|batch| {
                batch
                    .column(0)
                    .as_any()
                    .downcast_ref::<Int64Array>()
                    .unwrap()
                    .values()
                    .to_vec()
            })
            .unwrap_or_default())
    }

    #[test]
    fn fixture_where_supports_or_not_parentheses_in_between_and_like_escape() {
        let ids = |clause: &str| orders_where_ids(clause).unwrap();
        assert_eq!(ids("id = 1 OR id = 3"), vec![1, 3]);
        // AND binds tighter than OR; parentheses override.
        assert_eq!(ids("id = 1 OR id = 2 AND id = 3"), vec![1]);
        assert_eq!(ids("(id = 1 OR id = 2) AND id = 2"), vec![2]);
        assert_eq!(ids("NOT (id > 2) AND id <> 1"), vec![2]);
        assert_eq!(ids("\"id\" IN (3, 5, 7)"), vec![3, 5, 7]);
        assert_eq!(ids("\"id\" NOT IN (1,2,3) AND id < 6"), vec![4, 5]);
        assert_eq!(ids("id BETWEEN 4 AND 6"), vec![4, 5, 6]);
        assert_eq!(ids("(id NOT BETWEEN 2 AND 239)"), vec![1, 240]);
        // Text literals compare to numbers like PostgreSQL's unknown literals.
        assert_eq!(ids("id = '7'"), vec![7]);
        // SQL three-valued logic: NULL status never matches IN / NOT IN / =.
        let pending = ids("status = 'pending'");
        assert!(!pending.contains(&1), "id 1 has a NULL status");
        let not_active = ids("status NOT IN ('active')");
        assert_eq!(not_active, pending, "NULL rows are excluded from NOT IN");
        assert_eq!(ids("status IS NULL").len(), 22);
        assert_eq!(
            ids("(status IS NULL OR status IN ('pending')) AND id < 3"),
            vec![1, 2]
        );
        // Quoted identifiers and a PostgreSQL ::text cast for LIKE on numbers.
        assert_eq!(ids("\"id\"::text LIKE '24%' ESCAPE '!'"), vec![24, 240]);
        assert_eq!(ids("\"id\"::text LIKE '%0' ESCAPE '!'").len(), 24);
    }

    #[test]
    fn fixture_like_honours_the_escape_character() {
        let ids = |clause: &str| orders_where_ids(clause).unwrap();
        // customer-017 .. : `_` is a wildcard unless escaped.
        assert_eq!(ids("name LIKE 'customer_017' ESCAPE '!'"), vec![18]);
        assert!(ids("name LIKE 'customer!_017' ESCAPE '!'").is_empty());
        assert!(ids("name LIKE '%!%%' ESCAPE '!'").is_empty(), "no literal percent signs");
        assert_eq!(ids("name LIKE 'customer-01!%' ESCAPE '!'").len(), 0);
        assert_eq!(ids("name LIKE 'customer-01%' ESCAPE '!'").len(), 10);
        assert_eq!(ids("name NOT LIKE '%customer%' ESCAPE '!'"), vec![1, 2]);
        assert_eq!(ids("name LIKE '%!!%' ESCAPE '!'").len(), 0);
    }

    #[test]
    fn fixture_where_reports_engine_style_errors() {
        let error = |clause: &str| orders_where_ids(clause).unwrap_err();
        assert!(error("nope = 1").contains("column \"nope\" does not exist"));
        assert!(error("id = 'abc'").contains("invalid input syntax"));
        assert!(error("id >").contains("syntax error"));
        assert!(error("id IN ()").contains("syntax error"));
        assert!(error("(id = 1").contains("syntax error"));
        assert!(error("id = 1 id").contains("syntax error"));
    }

    /// The builder's SQL runs through the same fixture evaluator, so rules,
    /// generated SQL and result rows are checked together (every mode).
    #[test]
    fn generated_filter_sql_selects_the_expected_fixture_rows() {
        use crate::sql_filter::{
            build_where, SqlFilterColumnKind as Kind, SqlFilterMode as Mode, SqlFilterRule,
        };
        use crate::sql_table::SqlDialect;
        let kind = |column: &str| match column {
            "id" | "score" => Kind::Number,
            "active" => Kind::Boolean,
            _ => Kind::Text,
        };
        let run = |column: &str, mode: Mode, value: &str, end: &str| {
            let rule = SqlFilterRule {
                id: 1,
                column: column.into(),
                mode,
                value: value.into(),
                end_value: end.into(),
                ..Default::default()
            };
            let sql = build_where(SqlDialect::POSTGRES, &[rule], kind).unwrap();
            let ids = orders_where_ids(&sql)
                .unwrap_or_else(|error| panic!("{mode:?} {sql}: {error}"));
            (sql, ids)
        };
        assert_eq!(run("id", Mode::Equals, "5", "").1, vec![5]);
        assert_eq!(run("id", Mode::NotEquals, "5", "").1.len(), 239);
        assert_eq!(run("id", Mode::LessThan, "3", "").1, vec![1, 2]);
        assert_eq!(run("id", Mode::LessThanOrEqual, "3", "").1, vec![1, 2, 3]);
        assert_eq!(run("id", Mode::GreaterThan, "238", "").1, vec![239, 240]);
        assert_eq!(run("id", Mode::GreaterThanOrEqual, "239", "").1, vec![239, 240]);
        assert_eq!(run("status", Mode::IsNull, "", "").1.len(), 22);
        assert_eq!(run("status", Mode::IsNotNull, "", "").1.len(), 218);
        assert_eq!(run("status", Mode::IsBlank, "", "").1.len(), 22, "blank = NULL or ''");
        assert_eq!(run("status", Mode::IsNotBlank, "", "").1.len(), 218);
        assert_eq!(run("name", Mode::Like, "mer-02", "").1.len(), 10);
        assert_eq!(run("name", Mode::NotLike, "customer", "").1, vec![1, 2]);
        assert_eq!(run("name", Mode::BeginsWith, "customer-00", "").1.len(), 8, "indexes 2..=9");
        assert_eq!(run("name", Mode::EndsWith, "-239", "").1, vec![240]);
        assert_eq!(run("id", Mode::In, "1, 2\n3", "").1, vec![1, 2, 3]);
        assert_eq!(run("id", Mode::NotIn, "1,2", "").1.len(), 238);
        assert_eq!(run("id", Mode::Between, "10", "12").1, vec![10, 11, 12]);
        assert_eq!(run("id", Mode::NotBetween, "2", "239").1, vec![1, 240]);
        assert_eq!(run("active", Mode::Equals, "true", "").1.len(), 120);
        // Id column through `LIKE` needs the PostgreSQL ::text cast.
        let (sql, ids) = run("id", Mode::BeginsWith, "24", "");
        assert!(sql.contains("::text"), "{sql}");
        assert_eq!(ids, vec![24, 240]);
        // Nasty values select nothing and never error (all stay literals).
        for nasty in [
            "'", "x' OR '1'='1", "a'; DROP TABLE orders; --", "100%", "snake_case", "back\\slash", "日本語",
        ] {
            for mode in [Mode::Equals, Mode::Like, Mode::BeginsWith, Mode::In, Mode::NotLike] {
                let (sql, ids) = run("name", mode, nasty, "");
                if mode == Mode::NotLike {
                    assert_eq!(ids.len(), 240, "{sql}");
                } else {
                    assert!(ids.is_empty(), "{mode:?} {nasty:?} matched {ids:?}: {sql}");
                }
            }
        }
    }

    #[test]
    fn fixture_filter_and_sort_are_applied_before_paging() {
        let fixture = UiFixture::new(FixtureKind::Sample);
        let page = fixture
            .query_page_with_options(
                MONGO_ID,
                "analytics",
                "",
                "events",
                0,
                3,
                Some(r#"{"active":false}"#),
                Some(r#"{"id":-1}"#),
            )
            .expect("filtered fixture page");
        let ids = page.batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        assert_eq!(page.total_count, 120);
        assert_eq!(ids.values(), &[240, 238, 236]);
    }

    #[test]
    fn fixture_updates_are_atomic_and_scoped_to_full_identity() {
        let mut fixture = UiFixture::new(FixtureKind::Sample);
        let mixed = vec![
            UpdateCellRequest {
                database: "warehouse".into(),
                schema: "public".into(),
                table: "orders".into(),
                key_column: "id".into(),
                key_value: MutationValue::Number("3".into()),
                column: "name".into(),
                value: MutationValue::Text("must not persist".into()),
            },
            UpdateCellRequest {
                database: "warehouse".into(),
                schema: "public".into(),
                table: "orders".into(),
                key_column: "id".into(),
                key_value: MutationValue::Number("3".into()),
                column: "created".into(),
                value: MutationValue::Text("2026-01-01".into()),
            },
        ];
        assert!(fixture
            .apply_sql_updates(POSTGRES_ID, "warehouse", "public", "orders", &mixed)
            .is_err());
        let unchanged = fixture
            .query_page(POSTGRES_ID, "warehouse", "public", "orders", 2, 1)
            .unwrap();
        assert_eq!(
            unchanged.batches[0]
                .column(1)
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap()
                .value(0),
            "customer-002"
        );

        let valid = vec![
            UpdateCellRequest {
                column: "name".into(),
                value: MutationValue::Text("isolated row".into()),
                ..mixed[0].clone()
            },
            UpdateCellRequest {
                column: "big_number".into(),
                value: MutationValue::Number("18446744073709551600".into()),
                ..mixed[0].clone()
            },
        ];
        assert_eq!(
            fixture
                .apply_sql_updates(POSTGRES_ID, "warehouse", "public", "orders", &valid)
                .unwrap(),
            2
        );
        let persisted = fixture
            .query_page(POSTGRES_ID, "warehouse", "public", "orders", 2, 1)
            .unwrap();
        assert_eq!(
            persisted.batches[0]
                .column(1)
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap()
                .value(0),
            "isolated row"
        );
        let other_object = fixture
            .query_page(POSTGRES_ID, "warehouse", "public", "customers", 2, 1)
            .unwrap();
        assert_eq!(
            other_object.batches[0]
                .column(1)
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap()
                .value(0),
            "customer-002"
        );
        let other_connection = fixture
            .query_page(MONGO_ID, "warehouse", "public", "orders", 2, 1)
            .unwrap();
        assert_eq!(
            other_connection.batches[0]
                .column(1)
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap()
                .value(0),
            "customer-002"
        );
    }

    #[test]
    fn mongo_pages_preserve_bson_types_and_row_snapshot_identity() {
        let mut fixture = UiFixture::new(FixtureKind::Sample);
        let page = fixture
            .query_mongo_page(
                MONGO_ID,
                "fixture_db_0",
                "fixture_collection_00",
                0,
                2,
                None,
                None,
            )
            .expect("Mongo fixture page");
        assert_eq!(page.page.row_count, 2);
        assert_eq!(page.page.columns[0].name, "_id");
        assert_eq!(page.page.columns[0].data_type, "object_id");
        assert_eq!(page.page.columns[5].data_type, "date_time");
        assert_eq!(page.page.columns[6].data_type, "decimal128");
        assert_eq!(
            page.page.columns[2].data_type, "string",
            "a leading null must not hide the field's value type"
        );

        let row = fixture
            .inspect_mongo_row(page.page.page_id, 0)
            .expect("Mongo row");
        assert_eq!(row.page_id, page.page.page_id);
        assert_eq!(row.row_index, 0);
        assert_eq!(
            row.mutation_identity["_id"]["$oid"],
            "000000000000000000000001"
        );
        assert!(row
            .fields
            .iter()
            .any(|field| field.native_type == "decimal128"));
    }

    #[test]
    fn empty_mongo_profile_keeps_bson_columns_without_rows() {
        let mut fixture = UiFixture::new(FixtureKind::Empty);
        let result = fixture
            .query_mongo_page(MONGO_ID, "fixture_db_0", "events", 0, 50, None, None)
            .expect("empty BSON fixture");
        assert_eq!(result.page.row_count, 0);
        assert_eq!(result.page.total_count, Some(0));
        assert!(!result.page.columns.is_empty());
        assert!(fixture.inspect_mongo_row(result.page.page_id, 0).is_err());
    }

    #[test]
    fn mongo_fixture_uses_canonical_extended_json_types() {
        assert_eq!(
            json_to_bson(&serde_json::json!({"$date": {"$numberLong": "0"}})).unwrap(),
            Bson::DateTime(DateTime::from_millis(0))
        );
        let value =
            json_to_bson(&serde_json::json!({"$binary": {"base64": "AQID", "subType": "00"}}))
                .unwrap();
        assert!(matches!(value, Bson::Binary(binary) if binary.bytes == [1, 2, 3]));
    }

    #[test]
    fn mongo_mutations_are_atomic_and_persist_by_collection_identity() {
        let mut fixture = UiFixture::new(FixtureKind::Sample);
        let page = fixture
            .query_mongo_page(
                MONGO_ID,
                "fixture_db_0",
                "fixture_collection_00",
                0,
                1,
                None,
                None,
            )
            .expect("Mongo fixture page");
        let row = fixture.inspect_mongo_row(page.page.page_id, 0).unwrap();
        let update = MutationRequest {
            operation_id: OperationId(1),
            kind: "update".into(),
            database: Some("fixture_db_0".into()),
            object: Some("fixture_collection_00".into()),
            identity: row.mutation_identity,
            payload: serde_json::json!({"$set": {"name": "updated mongo fixture"}}),
        };
        assert_eq!(
            fixture
                .apply_mongo_mutations(
                    MONGO_ID,
                    "fixture_db_0",
                    "fixture_collection_00",
                    std::slice::from_ref(&update),
                )
                .unwrap(),
            1
        );
        let updated = fixture
            .query_mongo_page(
                MONGO_ID,
                "fixture_db_0",
                "fixture_collection_00",
                0,
                1,
                Some(r#"{"name":"updated mongo fixture"}"#),
                None,
            )
            .unwrap();
        assert_eq!(updated.page.total_count, Some(1));

        let invalid = MutationRequest {
            payload: serde_json::json!({"$set": {"_id": "not writable"}}),
            ..update
        };
        assert!(fixture
            .apply_mongo_mutations(
                MONGO_ID,
                "fixture_db_0",
                "fixture_collection_00",
                &[invalid],
            )
            .is_err());
        let unchanged = fixture
            .query_mongo_page(
                MONGO_ID,
                "fixture_db_0",
                "fixture_collection_00",
                0,
                1,
                None,
                None,
            )
            .unwrap();
        let unchanged_row = fixture
            .inspect_mongo_row(unchanged.page.page_id, 0)
            .unwrap();
        assert_eq!(
            unchanged_row
                .fields
                .iter()
                .find(|field| field.name == "name")
                .unwrap()
                .value,
            "updated mongo fixture"
        );
    }

    #[test]
    fn mongo_fixture_catalog_applies_admin_operations_and_refuses_like_the_driver() {
        use crate::mongo_admin::{IndexKeyType, MongoAdminOp, NewIndex};
        let mut fixture = UiFixture::new(FixtureKind::Sample);
        let apply = |fixture: &mut UiFixture, database: &str, op: MongoAdminOp| {
            fixture.apply_mongo_admin(MONGO_ID, database, &op)
        };
        let names = |fixture: &UiFixture, database: &str| -> Vec<String> {
            fixture.mongo_catalog.collections[database].clone()
        };
        let db = "fixture_db_0";
        // Rename keeps the collection set consistent and moves its indexes.
        assert_eq!(
            apply(&mut fixture, db, MongoAdminOp::RenameCollection {
                from: "fixture_collection_00".into(),
                to: "renamed".into(),
            }),
            Ok(1)
        );
        assert!(names(&fixture, db).contains(&"renamed".to_string()));
        assert!(!names(&fixture, db).contains(&"fixture_collection_00".to_string()));
        assert_eq!(fixture.list_mongo_indexes(MONGO_ID, db, "renamed").unwrap().len(), 3);
        // Refusals: existing target, bad name, missing source, same name.
        for (from, to) in [
            ("renamed", "fixture_collection_01"),
            ("renamed", "bad$name"),
            ("missing", "fresh"),
            ("renamed", "renamed"),
        ] {
            assert!(
                apply(&mut fixture, db, MongoAdminOp::RenameCollection {
                    from: from.into(),
                    to: to.into(),
                })
                .is_err(),
                "{from} -> {to}"
            );
        }
        // Clone: documents counted, indexes only when asked.
        assert_eq!(
            apply(&mut fixture, db, MongoAdminOp::CloneCollection {
                from: "renamed".into(),
                to: "with_indexes".into(),
                include_indexes: true,
            }),
            Ok(240)
        );
        apply(&mut fixture, db, MongoAdminOp::CloneCollection {
            from: "renamed".into(),
            to: "data_only".into(),
            include_indexes: false,
        })
        .unwrap();
        assert_eq!(fixture.list_mongo_indexes(MONGO_ID, db, "with_indexes").unwrap().len(), 3);
        assert_eq!(fixture.list_mongo_indexes(MONGO_ID, db, "data_only").unwrap().len(), 1);
        // Indexes: create, duplicate name refused, _id_ undroppable, drop.
        let index = NewIndex::from_form(
            &[("status".into(), IndexKeyType::Descending)],
            "",
            true,
            false,
            "",
        )
        .unwrap();
        let create = |fixture: &mut UiFixture| {
            apply(fixture, db, MongoAdminOp::CreateIndex {
                collection: "renamed".into(),
                index: index.clone(),
            })
        };
        assert_eq!(create(&mut fixture), Ok(1));
        assert!(create(&mut fixture).is_err(), "duplicate index name");
        let drop_index = |fixture: &mut UiFixture, name: &str| {
            apply(fixture, db, MongoAdminOp::DropIndex {
                collection: "renamed".into(),
                name: name.into(),
            })
        };
        assert!(drop_index(&mut fixture, "_id_").is_err());
        assert!(drop_index(&mut fixture, "nope").is_err());
        assert_eq!(drop_index(&mut fixture, "status_-1"), Ok(1));
        // Create database: it lists, a second create is refused like the driver's.
        assert_eq!(apply(&mut fixture, "fresh_db", MongoAdminOp::CreateDatabase), Ok(1));
        assert!(apply(&mut fixture, "fresh_db", MongoAdminOp::CreateDatabase).is_err());
        assert_eq!(names(&fixture, "fresh_db"), ["tablex_init"]);
        assert_eq!(apply(&mut fixture, "fresh_db", MongoAdminOp::DropDatabase), Ok(1));
        // Drop collection then database; system databases are protected.
        assert_eq!(
            apply(&mut fixture, db, MongoAdminOp::DropCollection { collection: "renamed".into() }),
            Ok(1)
        );
        assert!(fixture.list_mongo_indexes(MONGO_ID, db, "renamed").is_err());
        assert!(apply(&mut fixture, "admin", MongoAdminOp::DropDatabase).is_err());
        assert_eq!(apply(&mut fixture, db, MongoAdminOp::DropDatabase), Ok(1));
        assert!(apply(&mut fixture, db, MongoAdminOp::DropDatabase).is_err());
        let root = fixture
            .load(&LoadRequest::Provider {
                connection: MONGO_ID.into(),
                parent: None,
                payload: None,
                cursor: None,
                limit: 200,
            })
            .unwrap();
        let FixtureLoad::Provider(root) = root else {
            panic!("provider root");
        };
        assert!(root.nodes.iter().all(|node| node.label != db));
        assert_eq!(
            fixture.mongo_admin_operations(),
            [
                "rename_collection",
                "clone_collection",
                "clone_collection",
                "create_index",
                "drop_index",
                "create_database",
                "drop_database",
                "drop_collection",
                "drop_database",
            ]
        );
        // Non-Mongo connections are refused.
        assert!(fixture
            .apply_mongo_admin(POSTGRES_ID, "x", &MongoAdminOp::DropDatabase)
            .is_err());
    }

    #[test]
    fn mongo_provider_pages_use_server_caps_and_validate_opaque_cursors() {
        let fixture = UiFixture::new(FixtureKind::Sample);
        let root = fixture
            .load(&LoadRequest::Provider {
                connection: MONGO_ID.into(),
                parent: None,
                payload: None,
                cursor: None,
                limit: 200,
            })
            .unwrap();
        let FixtureLoad::Provider(root) = root else {
            panic!("expected provider root page");
        };
        assert_eq!(root.nodes.len(), 4);
        let root_cursor = root.next_cursor.clone().expect("root continuation");
        let next_root = fixture
            .load(&LoadRequest::Provider {
                connection: MONGO_ID.into(),
                parent: None,
                payload: None,
                cursor: Some(root_cursor),
                limit: 200,
            })
            .unwrap();
        let FixtureLoad::Provider(next_root) = next_root else {
            panic!("expected provider root continuation");
        };
        assert_eq!(next_root.nodes.len(), 4);
        assert!(next_root.next_cursor.is_none());

        let first_collections = fixture
            .load(&LoadRequest::Provider {
                connection: MONGO_ID.into(),
                parent: Some("database:fixture_db_0".into()),
                payload: Some(serde_json::json!({"database": "fixture_db_0"})),
                cursor: None,
                limit: 200,
            })
            .unwrap();
        let FixtureLoad::Provider(first_collections) = first_collections else {
            panic!("expected provider collection page");
        };
        assert_eq!(first_collections.nodes.len(), 10);
        let cursor = first_collections
            .next_cursor
            .expect("collection continuation");
        let second_collections = fixture
            .load(&LoadRequest::Provider {
                connection: MONGO_ID.into(),
                parent: Some("database:fixture_db_0".into()),
                payload: Some(serde_json::json!({"database": "fixture_db_0"})),
                cursor: Some(cursor.clone()),
                limit: 200,
            })
            .unwrap();
        let FixtureLoad::Provider(second_collections) = second_collections else {
            panic!("expected provider collection continuation");
        };
        assert_eq!(second_collections.nodes.len(), 10);
        let cursor = second_collections
            .next_cursor
            .expect("final collection continuation");
        let final_collections = fixture
            .load(&LoadRequest::Provider {
                connection: MONGO_ID.into(),
                parent: Some("database:fixture_db_0".into()),
                payload: Some(serde_json::json!({"database": "fixture_db_0"})),
                cursor: Some(cursor.clone()),
                limit: 200,
            })
            .unwrap();
        let FixtureLoad::Provider(final_collections) = final_collections else {
            panic!("expected final provider collection page");
        };
        assert_eq!(final_collections.nodes.len(), 5);
        assert!(final_collections.next_cursor.is_none());
        assert!(fixture
            .load(&LoadRequest::Provider {
                connection: MONGO_ID.into(),
                parent: Some("database:other".into()),
                payload: Some(serde_json::json!({"database": "other"})),
                cursor: Some(cursor),
                limit: 200,
            })
            .is_err());
    }

    #[test]
    fn mongo_empty_pages_keep_typed_columns() {
        let mut fixture = UiFixture::new(FixtureKind::Sample);
        let page = fixture
            .query_mongo_page(
                MONGO_ID,
                "fixture_db_0",
                "fixture_collection_00",
                0,
                0,
                None,
                None,
            )
            .unwrap();
        assert_eq!(page.page.row_count, 0);
        assert_eq!(page.page.total_count, Some(240));
        assert_eq!(page.page.columns[0].data_type, "object_id");
        assert_eq!(page.page.columns[6].data_type, "decimal128");
    }

    #[test]
    fn mongo_page_snapshot_lives_with_the_last_lease_and_then_expires() {
        let mut fixture = UiFixture::new(FixtureKind::Sample);
        let page = fixture
            .query_mongo_page(
                MONGO_ID,
                "fixture_db_0",
                "fixture_collection_00",
                0,
                1,
                None,
                None,
            )
            .unwrap();
        let page_id = page.page.page_id;
        let clone = page.lease.clone();
        drop(page);
        assert!(fixture.inspect_mongo_row(page_id, 0).is_ok());
        drop(clone);
        assert!(fixture.inspect_mongo_row(page_id, 0).is_err());
        let _ = fixture
            .query_mongo_page(
                MONGO_ID,
                "fixture_db_0",
                "fixture_collection_00",
                0,
                1,
                None,
                None,
            )
            .unwrap();
        assert!(!fixture.mongo_pages.contains_key(&page_id));
    }

    #[test]
    fn failed_mongo_insert_batch_does_not_publish_id_allocator_state() {
        let mut fixture = UiFixture::new(FixtureKind::Sample);
        let insert = MutationRequest {
            operation_id: OperationId(2),
            kind: "insert".into(),
            database: Some("fixture_db_0".into()),
            object: Some("fixture_collection_00".into()),
            identity: serde_json::json!({}),
            payload: serde_json::json!({"name": "allocated after failure"}),
        };
        let invalid = MutationRequest {
            kind: "update".into(),
            identity: serde_json::json!({"_id": {"$oid": "000000000000000000000999"}}),
            payload: serde_json::json!({"$set": {"name": "must fail"}}),
            ..insert.clone()
        };
        assert!(fixture
            .apply_mongo_mutations(
                MONGO_ID,
                "fixture_db_0",
                "fixture_collection_00",
                &[insert.clone(), invalid],
            )
            .is_err());
        assert_eq!(
            fixture
                .apply_mongo_mutations(
                    MONGO_ID,
                    "fixture_db_0",
                    "fixture_collection_00",
                    &[insert],
                )
                .unwrap(),
            1
        );
        let page = fixture
            .query_mongo_page(
                MONGO_ID,
                "fixture_db_0",
                "fixture_collection_00",
                0,
                1,
                Some(r#"{"name":"allocated after failure"}"#),
                None,
            )
            .unwrap();
        let row = fixture.inspect_mongo_row(page.page.page_id, 0).unwrap();
        assert_eq!(
            row.mutation_identity["_id"]["$oid"],
            "0000000000000000000000f1"
        );
    }

    #[test]
    fn mongo_delete_then_insert_allocates_a_unique_monotonic_id() {
        let mut fixture = UiFixture::new(FixtureKind::Sample);
        let page = fixture
            .query_mongo_page(
                MONGO_ID,
                "fixture_db_0",
                "fixture_collection_00",
                0,
                1,
                None,
                None,
            )
            .unwrap();
        let row = fixture.inspect_mongo_row(page.page.page_id, 0).unwrap();
        let delete = MutationRequest {
            operation_id: OperationId(3),
            kind: "delete".into(),
            database: Some("fixture_db_0".into()),
            object: Some("fixture_collection_00".into()),
            identity: row.mutation_identity,
            payload: serde_json::json!({}),
        };
        let insert = MutationRequest {
            kind: "insert".into(),
            identity: serde_json::json!({}),
            payload: serde_json::json!({"name": "after delete"}),
            ..delete.clone()
        };
        assert_eq!(
            fixture
                .apply_mongo_mutations(
                    MONGO_ID,
                    "fixture_db_0",
                    "fixture_collection_00",
                    &[delete, insert],
                )
                .unwrap(),
            2
        );
        let page = fixture
            .query_mongo_page(
                MONGO_ID,
                "fixture_db_0",
                "fixture_collection_00",
                0,
                1,
                Some(r#"{"name":"after delete"}"#),
                None,
            )
            .unwrap();
        let row = fixture.inspect_mongo_row(page.page.page_id, 0).unwrap();
        assert_eq!(
            row.mutation_identity["_id"]["$oid"],
            "0000000000000000000000f1"
        );
    }
}
