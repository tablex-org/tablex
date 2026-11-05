//! Canonical Extended JSON text <-> BSON, and shell text for display.

use bson::{Bson, Document};

#[derive(Debug, thiserror::Error)]
pub enum EjsonError {
    #[error("not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("not valid Extended JSON: {0}")]
    Bson(String),
    #[error("expected a document")]
    NotADocument,
}

/// Parses any Extended JSON value (canonical or relaxed).
pub fn parse_value(text: &str) -> Result<Bson, EjsonError> {
    let value: serde_json::Value = serde_json::from_str(text)?;
    Bson::try_from(value).map_err(|error| EjsonError::Bson(error.to_string()))
}

pub fn parse_document(text: &str) -> Result<Document, EjsonError> {
    match parse_value(text)? {
        Bson::Document(document) => Ok(document),
        _ => Err(EjsonError::NotADocument),
    }
}

/// Canonical Extended JSON, keys in document order.
pub fn value_to_text(value: &Bson) -> String {
    value.clone().into_canonical_extjson().to_string()
}

pub fn document_to_text(document: &Document) -> String {
    value_to_text(&Bson::Document(document.clone()))
}

/// The mongosh text of a value (`ObjectId('...')`, `{ a: 1 }`).
pub fn shell_text(value: &Bson, pretty: bool) -> String {
    crate::shell_value::to_shell(value, pretty)
}
