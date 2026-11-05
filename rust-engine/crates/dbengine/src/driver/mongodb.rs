use super::DatabaseDriver;
use crate::error::Result;
use async_trait::async_trait;
use mongodb::Client;
use std::sync::Arc;
use tracing::{debug, instrument};

pub struct MongoDBDriver {
    client: Arc<Client>,
}

impl MongoDBDriver {
    pub fn new(client: Arc<Client>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl DatabaseDriver for MongoDBDriver {
    #[instrument(skip(self))]
    async fn list_databases(&self) -> Result<Vec<String>> {
        let dbs = self.client.list_database_names().await?;
        debug!(count = dbs.len(), "Listed databases");
        Ok(dbs)
    }

    #[instrument(skip(self), fields(db = %db))]
    async fn list_collections(&self, db: &str) -> Result<Vec<String>> {
        let collections = self.client.database(db).list_collection_names().await?;
        debug!(db, count = collections.len(), "Listed collections");
        Ok(collections)
    }
}
