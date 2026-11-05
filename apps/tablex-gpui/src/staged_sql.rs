//! Row changes staged in a SQL result tab until Apply: a typed value and the
//! table + key each change addresses. Apply turns them into the driver's
//! `SqlBatch` (the tab names the connection, so a change carries none).

#[derive(Debug, Clone, PartialEq)]
pub enum MutationValue {
    Null,
    Text(String),
    Number(String),
    Boolean(bool),
}

#[derive(Debug, Clone, PartialEq)]
pub struct UpdateCellRequest {
    pub database: String,
    pub schema: String,
    pub table: String,
    pub key_column: String,
    pub key_value: MutationValue,
    pub column: String,
    pub value: MutationValue,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeleteRowRequest {
    pub database: String,
    pub schema: String,
    pub table: String,
    pub key_column: String,
    pub key_value: MutationValue,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InsertRowRequest {
    pub database: String,
    pub schema: String,
    pub table: String,
    pub values: Vec<(String, MutationValue)>,
}
