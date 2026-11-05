pub mod mongodb;
pub use mongodb::MongoDBDriver;

use crate::error::Result;
use async_trait::async_trait;

#[async_trait]
pub trait DatabaseDriver: Send + Sync {
    async fn list_databases(&self) -> Result<Vec<String>>;
    async fn list_collections(&self, db: &str) -> Result<Vec<String>>;
}
