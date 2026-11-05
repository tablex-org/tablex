use async_trait::async_trait;
use bson::{doc, Document};
use futures::StreamExt;
use mongodb::Client;
use std::sync::Arc;

use crate::db::{CursorState, DatabaseConnection, DbType, QueryResult};

/// Wraps an existing MongoDB Client to implement DatabaseConnection.
pub struct MongoConn {
    pub client: Arc<Client>,
    pub uri: String,
    pub name: String,
}

impl MongoConn {
    pub fn new(client: Arc<Client>, uri: String, name: String) -> Self {
        Self { client, uri, name }
    }
}

#[async_trait]
impl DatabaseConnection for MongoConn {
    fn db_type(&self) -> DbType {
        DbType::Mongo
    }

    async fn test(&self) -> Result<(), String> {
        self.client
            .list_database_names()
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    async fn list_databases(&self) -> Result<Vec<String>, String> {
        self.client
            .list_database_names()
            .await
            .map_err(|e| e.to_string())
    }

    async fn list_collections(&self, db: &str) -> Result<Vec<String>, String> {
        let database = self.client.database(db);
        database
            .list_collection_names()
            .await
            .map_err(|e| e.to_string())
    }

    async fn execute_query(
        &self,
        db: &str,
        table: &str,
        user_where: Option<&str>,
        user_order_by: Option<&str>,
        cursor: Option<&CursorState>,
        limit: i64,
    ) -> Result<QueryResult, String> {
        // For MongoDB, user_where and user_order_by are JSON strings
        let filter: Document = if let Some(w) = user_where {
            if w.trim().is_empty() || w.trim() == "{}" {
                doc! {}
            } else {
                // Parse JSON string to BSON Document
                let json_value: serde_json::Value =
                    serde_json::from_str(w).map_err(|e| format!("Invalid filter JSON: {}", e))?;
                bson::from_slice(&serde_json::to_vec(&json_value).unwrap_or_default())
                    .map_err(|e| format!("Invalid filter BSON: {}", e))?
            }
        } else {
            doc! {}
        };

        let sort: Option<Document> = if let Some(o) = user_order_by {
            if o.trim().is_empty() {
                None
            } else {
                let json_value: serde_json::Value =
                    serde_json::from_str(o).map_err(|e| format!("Invalid sort JSON: {}", e))?;
                Some(
                    bson::from_slice(&serde_json::to_vec(&json_value).unwrap_or_default())
                        .map_err(|e| format!("Invalid sort BSON: {}", e))?,
                )
            }
        } else {
            None
        };

        let database = self.client.database(db);
        let collection = database.collection::<Document>(table);

        // Handle cursor (use _id > last_value for MongoDB)
        let final_filter = if let Some(c) = cursor {
            if let Ok(oid) = bson::oid::ObjectId::parse_str(&c.last_value) {
                let cursor_filter = doc! { "_id": { "$gt": oid } };
                if filter.is_empty() {
                    cursor_filter
                } else {
                    doc! { "$and": [filter.clone(), cursor_filter] }
                }
            } else {
                filter.clone()
            }
        } else {
            filter.clone()
        };

        let find_opts = mongodb::options::FindOptions::builder()
            .limit(Some(limit))
            .sort(sort)
            .build();

        let mut cursor_result = collection
            .find(final_filter)
            .with_options(find_opts)
            .await
            .map_err(|e| e.to_string())?;

        let mut docs: Vec<Document> = Vec::new();
        while let Some(result) = cursor_result.next().await {
            if let Ok(doc) = result {
                docs.push(doc);
            }
        }

        let total_count = collection
            .count_documents(filter)
            .await
            .map_err(|e| e.to_string())?;

        // Build cursor for next page
        let next_cursor = if docs.len() as i64 >= limit {
            docs.last().and_then(|d| {
                d.get_object_id("_id").ok().map(|oid| CursorState {
                    order_column: "_id".to_string(),
                    last_value: oid.to_hex(),
                    pk_type: String::new(),
                })
            })
        } else {
            None
        };

        // Return empty batches - the actual Arrow conversion happens in lib.rs
        // using documents_to_arrow_cdata
        Ok(QueryResult {
            batches: vec![],
            total_count: total_count as i64,
            cursor: next_cursor,
        })
    }

    async fn cancel(&self) {
        // MongoDB doesn't have a simple cancel mechanism via FFI
    }
}
