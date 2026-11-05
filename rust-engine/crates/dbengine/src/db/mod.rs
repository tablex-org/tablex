use arrow_array::RecordBatch;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub mod mongo;
pub mod mysql;
pub mod mysql_arrow;
mod pg_transport;
pub mod postgres;
pub mod postgres_arrow;
pub mod sql_builder;
pub mod sqlite;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbType {
    Mongo,
    Postgres,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CursorState {
    pub order_column: String,
    pub last_value: String,
    /// PostgreSQL type OID string, e.g. "int4", "int8", "text", "uuid".
    /// Used to emit the correct cast in SQL: `pk > $1::int8`.
    /// Empty string means no cast (string comparison).
    #[serde(default)]
    pub pk_type: String,
}

#[derive(Debug, Clone)]
pub struct QueryResult {
    pub batches: Vec<RecordBatch>,
    pub total_count: i64,
    pub cursor: Option<CursorState>,
}

#[async_trait]
pub trait DatabaseConnection: Send + Sync {
    fn db_type(&self) -> DbType;

    async fn test(&self) -> Result<(), String>;

    async fn list_databases(&self) -> Result<Vec<String>, String>;

    async fn list_collections(&self, db: &str) -> Result<Vec<String>, String>;

    async fn execute_query(
        &self,
        db: &str,
        table: &str,
        user_where: Option<&str>,
        user_order_by: Option<&str>,
        cursor: Option<&CursorState>,
        limit: i64,
    ) -> Result<QueryResult, String>;

    async fn cancel(&self);
}
