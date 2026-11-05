use crate::error::{DbEngineError, Result};
use mongodb::Client;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, instrument};

pub struct ConnectionInfo {
    #[allow(dead_code)]
    pub id: u64,
    #[allow(dead_code)]
    pub uri: String,
    #[allow(dead_code)]
    pub name: String,
    pub client: Arc<Client>,
}

pub struct ConnectionManager {
    connections: HashMap<u64, Arc<RwLock<ConnectionInfo>>>,
    next_id: u64,
}

impl ConnectionManager {
    pub fn new() -> Self {
        Self {
            connections: HashMap::new(),
            next_id: 1,
        }
    }

    /// Insert an already-verified client with a caller-supplied ID.
    /// Used by `dbengine_connect` which manages the global ID counter.
    pub fn insert_client_with_id(
        &mut self,
        id: u64,
        uri: String,
        name: String,
        client: Arc<Client>,
    ) {
        let info = Arc::new(RwLock::new(ConnectionInfo {
            id,
            uri,
            name,
            client,
        }));
        self.connections.insert(id, info);
        info!(conn_id = id, "Client inserted into manager");
    }

    /// Insert an already-verified client. Returns the assigned connection ID.
    /// This is a sync operation — the async connection work is done by the caller.
    pub fn insert_client(&mut self, uri: String, name: String, client: Arc<Client>) -> u64 {
        let id = self.next_id;
        self.next_id += 1;

        let info = Arc::new(RwLock::new(ConnectionInfo {
            id,
            uri,
            name,
            client,
        }));

        self.connections.insert(id, info);
        info!(conn_id = id, "Client inserted into manager");
        id
    }

    /// Remove a client from the manager. Sync operation.
    pub fn remove_client(&mut self, conn_id: u64) -> Result<()> {
        self.connections.remove(&conn_id).ok_or_else(|| {
            DbEngineError::Connection(format!("Connection {} not found", conn_id))
        })?;
        info!(conn_id = conn_id, "Client removed from manager");
        Ok(())
    }

    #[allow(dead_code)]
    #[instrument(skip(self, uri), fields(name = %name))]
    pub async fn connect(&mut self, uri: String, name: String) -> Result<u64> {
        use mongodb::options::ClientOptions;

        let options = ClientOptions::parse(&uri)
            .await
            .map_err(|e| DbEngineError::Connection(e.to_string()))?;

        let client = Arc::new(
            Client::with_options(options).map_err(|e| DbEngineError::Connection(e.to_string()))?,
        );

        client
            .list_database_names()
            .await
            .map_err(|e| DbEngineError::Connection(e.to_string()))?;

        let id = self.next_id;
        self.next_id += 1;

        let info = Arc::new(RwLock::new(ConnectionInfo {
            id,
            uri,
            name,
            client,
        }));

        self.connections.insert(id, info);
        info!(conn_id = id, "Connection established");
        Ok(id)
    }

    #[allow(dead_code)]
    #[instrument(skip(self), fields(conn_id = %conn_id))]
    pub async fn disconnect(&mut self, conn_id: u64) -> Result<()> {
        self.connections.remove(&conn_id).ok_or_else(|| {
            DbEngineError::Connection(format!("Connection {} not found", conn_id))
        })?;
        info!("Connection disconnected");
        Ok(())
    }

    pub fn get_client(&self, conn_id: u64) -> Result<Arc<Client>> {
        let info = self.connections.get(&conn_id).ok_or_else(|| {
            DbEngineError::Connection(format!("Connection {} not found", conn_id))
        })?;

        let client = info
            .try_read()
            .map_err(|e| DbEngineError::EngineLock(e.to_string()))?
            .client
            .clone();

        Ok(client)
    }
}
