use std::sync::PoisonError;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum DbEngineError {
    #[error("Connection error: {0}")]
    Connection(String),

    #[error("Query error: {0}")]
    Query(String),

    #[error("Create database rejected: {0}")]
    CreateRejected(String),

    #[error("Serialization error: {0}")]
    Serialization(String),

    #[error("MongoDB error: {0}")]
    MongoDB(String),

    #[error("Engine lock poisoned: {0}")]
    EngineLock(String),

    #[allow(dead_code)]
    #[error("Operation timed out: {0}")]
    Timeout(String),
}

impl From<mongodb::error::Error> for DbEngineError {
    fn from(err: mongodb::error::Error) -> Self {
        DbEngineError::MongoDB(err.to_string())
    }
}

impl From<serde_json::Error> for DbEngineError {
    fn from(err: serde_json::Error) -> Self {
        DbEngineError::Serialization(err.to_string())
    }
}

impl<T> From<PoisonError<T>> for DbEngineError {
    fn from(err: PoisonError<T>) -> Self {
        DbEngineError::EngineLock(err.to_string())
    }
}

pub type Result<T> = std::result::Result<T, DbEngineError>;
