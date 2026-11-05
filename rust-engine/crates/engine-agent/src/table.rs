//! Table structure as the agent tools return it (compact, model-readable),
//! built from the driver's `TableProperties`.

use crate::limits::truncate_text;
use crate::redact::Masks;
use crate::sanitize::{clean, clean_multiline, clean_name, MAX_COMMENT_CHARS, MAX_NAME_CHARS};
use crate::secret::SecretText;
use engine_driver_api::sql::{ConstraintKind, TableColumn, TableProperties};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnDescription {
    pub name: String,
    #[serde(rename = "type")]
    pub data_type: String,
    pub nullable: bool,
    pub primary_key: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexDescription {
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForeignKeyDescription {
    pub name: String,
    pub columns: Vec<String>,
    pub referenced_table: String,
    pub referenced_columns: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_delete: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_update: Option<String>,
}

/// Result of `describe_table`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableDescription {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    pub columns: Vec<ColumnDescription>,
    /// Primary-key columns, in key order.
    pub primary_key: Vec<String>,
    /// Columns that are a unique key on their own (besides the primary key).
    pub unique_columns: Vec<String>,
    pub indexes: Vec<IndexDescription>,
    pub foreign_keys: Vec<ForeignKeyDescription>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ddl: Option<String>,
    /// The DDL was cut to the byte limit.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ddl_truncated: bool,
}

fn clean_names(names: &mut [String]) {
    for name in names {
        *name = clean_name(name, MAX_NAME_CHARS);
    }
}

impl TableDescription {
    /// Makes every database-supplied string safe to show: one line, no control
    /// or invisible characters, length-capped; defaults, comments and DDL are
    /// also scrubbed of secrets (URI credentials, `password=` values, exact
    /// `secrets`). DDL and defaults are otherwise returned as stored.
    pub fn sanitize(&mut self, secrets: &[SecretText]) {
        self.sanitize_with(&Masks::new(secrets));
    }

    /// [`Self::sanitize`] with the masks of the session's secrets prepared once, for a caller that
    /// sanitizes many tables (`get_schema_context`: up to hundreds of tables, two texts a column).
    pub fn sanitize_with(&mut self, masks: &Masks) {
        self.name = clean_name(&self.name, MAX_NAME_CHARS);
        self.database = self
            .database
            .take()
            .map(|name| clean_name(&name, MAX_NAME_CHARS));
        self.schema = self
            .schema
            .take()
            .map(|name| clean_name(&name, MAX_NAME_CHARS));
        for column in &mut self.columns {
            column.name = clean_name(&column.name, MAX_NAME_CHARS);
            column.data_type = clean(&column.data_type, MAX_NAME_CHARS);
            column.default = column
                .default
                .take()
                .map(|text| clean(&masks.redact(&text), MAX_COMMENT_CHARS));
            column.comment = column
                .comment
                .take()
                .map(|text| clean(&masks.redact(&text), MAX_COMMENT_CHARS));
        }
        clean_names(&mut self.primary_key);
        clean_names(&mut self.unique_columns);
        for index in &mut self.indexes {
            index.name = clean_name(&index.name, MAX_NAME_CHARS);
            clean_names(&mut index.columns);
        }
        for key in &mut self.foreign_keys {
            key.name = clean_name(&key.name, MAX_NAME_CHARS);
            key.referenced_table = clean_name(&key.referenced_table, MAX_NAME_CHARS);
            clean_names(&mut key.columns);
            clean_names(&mut key.referenced_columns);
            key.on_delete = key.on_delete.take().map(|text| clean(&text, 40));
            key.on_update = key.on_update.take().map(|text| clean(&text, 40));
        }
        self.ddl = self
            .ddl
            .take()
            .map(|ddl| clean_multiline(&masks.redact(&ddl)));
    }
}

impl TableDescription {
    /// Cuts the DDL to `max_ddl_bytes`, with a marker, and says so in `ddl_truncated`. Call it after
    /// [`Self::sanitize`]: the secrets are masked first, so a cut can never leave half of one.
    pub fn cap_ddl(&mut self, max_ddl_bytes: usize) {
        if let Some(cut) = self
            .ddl
            .as_deref()
            .and_then(|ddl| truncate_text(ddl, max_ddl_bytes))
        {
            self.ddl = Some(cut);
            self.ddl_truncated = true;
        }
    }
}

impl ColumnDescription {
    fn from_column(column: &TableColumn) -> Self {
        Self {
            name: column.name.clone(),
            data_type: column.data_type.clone(),
            nullable: column.nullable,
            primary_key: column.primary_key,
            default: column.default_expression.clone(),
            comment: column.comment.clone().filter(|comment| !comment.is_empty()),
        }
    }
}

impl TableDescription {
    /// Converts the driver's table properties. `ddl` is cut to `max_ddl_bytes`.
    pub fn from_properties(
        name: String,
        database: Option<String>,
        schema: Option<String>,
        properties: TableProperties,
        max_ddl_bytes: usize,
    ) -> Self {
        let columns: Vec<ColumnDescription> = properties
            .columns
            .iter()
            .map(ColumnDescription::from_column)
            .collect();
        let constraints = properties.keys.unwrap_or_default();
        let mut primary_key: Vec<String> = constraints
            .iter()
            .find(|key| key.kind == ConstraintKind::PrimaryKey)
            .map(|key| key.columns.clone())
            .unwrap_or_default();
        if primary_key.is_empty() {
            primary_key = columns
                .iter()
                .filter(|column| column.primary_key)
                .map(|column| column.name.clone())
                .collect();
        }
        let mut unique_columns: Vec<String> = constraints
            .iter()
            .filter(|key| key.kind == ConstraintKind::Unique && key.columns.len() == 1)
            .map(|key| key.columns[0].clone())
            .collect();
        unique_columns.sort();
        unique_columns.dedup();
        let indexes = properties
            .indexes
            .unwrap_or_default()
            .into_iter()
            .map(|index| IndexDescription {
                name: index.name,
                columns: index.columns,
                unique: index.unique,
            })
            .collect();
        let foreign_keys = properties
            .foreign_keys
            .unwrap_or_default()
            .into_iter()
            .map(|key| ForeignKeyDescription {
                name: key.name,
                columns: key.columns,
                referenced_table: key.referenced_table,
                referenced_columns: key.referenced_columns,
                on_delete: key.on_delete,
                on_update: key.on_update,
            })
            .collect();
        let (ddl, ddl_truncated) = match properties.ddl {
            Some(ddl) => match truncate_text(&ddl, max_ddl_bytes) {
                Some(cut) => (Some(cut), true),
                None => (Some(ddl), false),
            },
            None => (None, false),
        };
        Self {
            name,
            database,
            schema,
            columns,
            primary_key,
            unique_columns,
            indexes,
            foreign_keys,
            ddl,
            ddl_truncated,
        }
    }

    /// Structure from navigator column nodes only (no keys, indexes or DDL).
    pub fn from_columns(
        name: String,
        database: Option<String>,
        schema: Option<String>,
        columns: &[TableColumn],
    ) -> Self {
        Self::from_properties(
            name,
            database,
            schema,
            TableProperties {
                columns: columns.to_vec(),
                ..TableProperties::default()
            },
            0,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_driver_api::sql::{TableConstraint, TableForeignKey};

    fn column(name: &str, pk: bool) -> TableColumn {
        TableColumn {
            name: name.into(),
            data_type: "int".into(),
            nullable: !pk,
            primary_key: pk,
            ..TableColumn::default()
        }
    }

    #[test]
    fn properties_become_a_compact_description() {
        let properties = TableProperties {
            columns: vec![column("id", true), column("email", false)],
            keys: Some(vec![
                TableConstraint {
                    name: "pk".into(),
                    kind: ConstraintKind::PrimaryKey,
                    columns: vec!["id".into()],
                    definition: None,
                },
                TableConstraint {
                    name: "uq".into(),
                    kind: ConstraintKind::Unique,
                    columns: vec!["email".into()],
                    definition: None,
                },
            ]),
            foreign_keys: Some(vec![TableForeignKey {
                name: "fk".into(),
                table: "t".into(),
                columns: vec!["id".into()],
                referenced_table: "u".into(),
                referenced_columns: vec!["id".into()],
                on_update: None,
                on_delete: Some("CASCADE".into()),
                definition: None,
            }]),
            ddl: Some("x".repeat(100)),
            ..TableProperties::default()
        };
        let description = TableDescription::from_properties("t".into(), None, None, properties, 10);
        assert_eq!(description.primary_key, ["id"]);
        assert_eq!(description.unique_columns, ["email"]);
        assert_eq!(
            description.foreign_keys[0].on_delete.as_deref(),
            Some("CASCADE")
        );
        assert!(description.ddl_truncated);
        assert!(description.ddl.unwrap().contains("[truncated"));
    }

    #[test]
    fn the_primary_key_falls_back_to_the_column_flags() {
        let description = TableDescription::from_columns(
            "t".into(),
            None,
            None,
            &[column("a", true), column("b", true), column("c", false)],
        );
        assert_eq!(description.primary_key, ["a", "b"]);
        assert!(description.foreign_keys.is_empty());
    }
}
