//! Where a connection's own overrides (its agent access among them) are kept. The app stores
//! them in its connection records, the same place the connection dialog saves them; tests and
//! scenarios use memory, so they never touch the user's connection store.

#[cfg(any(test, feature = "ui-scenarios"))]
use std::collections::HashMap;
#[cfg(any(test, feature = "ui-scenarios"))]
use std::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordsError {
    /// The connection is not saved (any more).
    Gone,
    Storage(String),
}

impl std::fmt::Display for RecordsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Gone => f.write_str("the connection is no longer saved"),
            Self::Storage(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for RecordsError {}

/// Reads and writes the overrides text (`overrides_json`) of a saved connection. Blocking.
pub trait AgentRecords: Send + Sync {
    fn overrides_json(&self, connection: &str) -> Result<String, RecordsError>;
    fn save_overrides_json(&self, connection: &str, json: &str) -> Result<(), RecordsError>;
}

/// The app's connection records (`engine-storage`).
pub struct StorageRecords;

impl AgentRecords for StorageRecords {
    fn overrides_json(&self, connection: &str) -> Result<String, RecordsError> {
        engine_storage::connection::get_connection(connection)
            .map_err(|error| RecordsError::Storage(error.to_string()))?
            .map(|record| record.overrides_json)
            .ok_or(RecordsError::Gone)
    }

    fn save_overrides_json(&self, connection: &str, json: &str) -> Result<(), RecordsError> {
        let mut record = engine_storage::connection::get_connection(connection)
            .map_err(|error| RecordsError::Storage(error.to_string()))?
            .ok_or(RecordsError::Gone)?;
        record.overrides_json = json.to_owned();
        engine_storage::connection::upsert_connection(&record)
            .map_err(|error| RecordsError::Storage(error.to_string()))
    }
}

/// Overrides kept in memory.
#[cfg(any(test, feature = "ui-scenarios"))]
#[derive(Default)]
pub struct MemoryRecords {
    /// Connection id to overrides text; an id that is absent is "not saved".
    pub saved: Mutex<HashMap<String, String>>,
    /// The next write fails with this text.
    pub fail_next_write: Mutex<Option<String>>,
}

#[cfg(any(test, feature = "ui-scenarios"))]
impl MemoryRecords {
    #[cfg(test)]
    pub fn with(connection: &str, overrides_json: &str) -> Self {
        let records = Self::default();
        records
            .saved
            .lock()
            .unwrap()
            .insert(connection.to_owned(), overrides_json.to_owned());
        records
    }
}

#[cfg(any(test, feature = "ui-scenarios"))]
impl AgentRecords for MemoryRecords {
    fn overrides_json(&self, connection: &str) -> Result<String, RecordsError> {
        self.saved
            .lock()
            .unwrap()
            .get(connection)
            .cloned()
            .ok_or(RecordsError::Gone)
    }

    fn save_overrides_json(&self, connection: &str, json: &str) -> Result<(), RecordsError> {
        if let Some(why) = self.fail_next_write.lock().unwrap().take() {
            return Err(RecordsError::Storage(why));
        }
        let mut saved = self.saved.lock().unwrap();
        match saved.get_mut(connection) {
            Some(slot) => {
                *slot = json.to_owned();
                Ok(())
            }
            None => Err(RecordsError::Gone),
        }
    }
}
