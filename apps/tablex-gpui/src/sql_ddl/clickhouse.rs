//! ClickHouse DDL. Not transactional.
//!
//! A MergeTree table has a sorting key instead of a primary key, no foreign
//! keys and no secondary B-tree indexes (only data-skipping indices, which are
//! not offered here), and its key columns cannot be altered. Those operations
//! are refused with a message. A column's nullability is part of its type
//! (`Nullable(String)`), which is how the catalog reports it and how it is written.

use super::{
    column_list, default_operand, default_sql, ident, literal, objects, table_sql, type_operand, type_sql, Change, ColumnSpec,
    DdlCapabilities, DdlDialect, DdlError, ForeignKeySpec, NamespaceKind, ObjectCapabilities, TableId,
};
use crate::sql_table::SqlDialect;

pub(super) struct ClickHouse;

/// `Nullable(T)` for a nullable column, `T` itself otherwise (an already
/// `Nullable(...)` type is unwrapped for a NOT NULL column).
fn column_type(data_type: &str, nullable: bool) -> String {
    let data_type = data_type.trim();
    let inner = data_type.strip_prefix("Nullable(").and_then(|rest| rest.strip_suffix(')'));
    match (inner, nullable) {
        (Some(inner), false) => inner.trim().to_owned(),
        (Some(_), true) => data_type.to_owned(),
        (None, true) => format!("Nullable({data_type})"),
        (None, false) => data_type.to_owned(),
    }
}

impl ClickHouse {
    /// `` `name` Type [DEFAULT x] [COMMENT 'x'] ``
    fn column_definition(&self, column: &ColumnSpec) -> Result<String, DdlError> {
        let data_type = type_sql(self, &column.data_type)?;
        let mut sql = format!("{} {}", ident(self, "column", &column.name)?, column_type(&data_type, column.nullable));
        if let Some(default) = column.default.as_deref().filter(|d| !d.trim().is_empty()) {
            sql.push_str(&format!(" DEFAULT {}", default_sql(self, default)?));
        }
        if let Some(comment) = column.comment.as_deref().filter(|comment| !comment.is_empty()) {
            sql.push_str(&format!(" COMMENT {}", literal(self, comment)));
        }
        Ok(sql)
    }

    fn refuse(what: &str) -> DdlError {
        DdlError::Unsupported(format!("ClickHouse has no {what}; define it in SQL (MergeTree key and skipping-index syntax differ)."))
    }
}

impl DdlDialect for ClickHouse {
    fn flavor(&self) -> super::Flavor {
        super::Flavor::ClickHouse
    }

    fn label(&self) -> &'static str {
        "ClickHouse"
    }

    fn quoting(&self) -> SqlDialect {
        // `"…"` is a name here too (what the driver reports), and a backslash escapes in names.
        SqlDialect { double_quote_identifiers: true, ..SqlDialect::MYSQL }
    }

    /// A backslash inside backticks is an escape: `x\` followed by a backtick would close the name
    /// early, so every backslash is doubled (the backtick is doubled by the quoting itself).
    fn quote(&self, name: &str) -> String {
        self.quoting().quote(&name.replace('\\', "\\\\"))
    }

    fn null_to_not_null_rewrites(&self) -> bool {
        true
    }

    fn transactional(&self) -> bool {
        false
    }

    fn schema_is_database(&self) -> bool {
        true
    }

    fn max_identifier_length(&self) -> Option<usize> {
        None
    }

    fn capabilities(&self) -> DdlCapabilities {
        DdlCapabilities { indexes: false, foreign_keys: false, primary_key_edit: false, column_comments: true, alter_column: true }
    }

    fn object_capabilities(&self) -> ObjectCapabilities {
        ObjectCapabilities {
            drop_cascade: false,
            truncate_statement: true,
            truncate_options: false,
            rename_view: true,
            materialized_views: false,
            create_namespace: &[NamespaceKind::Database],
        }
    }

    fn common_types(&self) -> &'static [&'static str] {
        &["Int32", "Int64", "UInt32", "UInt64", "Float64", "Decimal(18, 2)", "String", "FixedString(16)", "Date", "DateTime", "UUID", "Bool"]
    }

    fn statements(&self, change: &Change, _inline: &[ForeignKeySpec]) -> Result<Vec<String>, DdlError> {
        Ok(match change {
            Change::CreateTable { table, columns, primary_key, comment } => {
                if columns.is_empty() {
                    return Err(DdlError::Invalid(format!("Table {table} has no column.")));
                }
                for key in primary_key {
                    if columns.iter().any(|column| column.name == *key && column.nullable) {
                        return Err(DdlError::Unsupported(format!(
                            "The key column {key} of {table} cannot be Nullable in a ClickHouse sorting key."
                        )));
                    }
                }
                let definitions =
                    columns.iter().map(|column| self.column_definition(column)).collect::<Result<Vec<_>, _>>()?;
                let order = if primary_key.is_empty() {
                    "tuple()".to_owned()
                } else {
                    format!("({})", column_list(self, "sorting key", primary_key)?)
                };
                let comment = match comment.as_deref().filter(|comment| !comment.is_empty()) {
                    Some(comment) => format!(" COMMENT {}", literal(self, comment)),
                    None => String::new(),
                };
                vec![format!(
                    "CREATE TABLE {} (\n  {}\n) ENGINE = MergeTree ORDER BY {order}{comment}",
                    table_sql(self, table)?,
                    definitions.join(",\n  ")
                )]
            }
            Change::DropTable { table, cascade } => vec![objects::drop_statement(self, "TABLE", table, *cascade)?],
            Change::DropView { table, materialized, cascade } => {
                vec![objects::drop_statement(self, objects::view_keyword(self, *materialized)?, table, *cascade)?]
            }
            Change::RenameTable { table, to } => {
                let target = TableId { schema: table.schema.clone(), name: to.clone() };
                vec![format!("RENAME TABLE {} TO {}", table_sql(self, table)?, table_sql(self, &target)?)]
            }
            Change::RenameView { table, to, materialized } => {
                objects::view_keyword(self, *materialized)?;
                objects::checked_name(self, "view", to)?;
                let target = TableId { schema: table.schema.clone(), name: to.clone() };
                vec![format!("RENAME TABLE {} TO {}", table_sql(self, table)?, table_sql(self, &target)?)]
            }
            Change::TruncateTable { table, restart_identity, cascade } => {
                objects::truncate_statements(self, table, *restart_identity, *cascade)?
            }
            Change::CreateNamespace { kind, name } => objects::create_namespace(self, *kind, name)?,
            Change::AddColumn { table, column } => {
                vec![format!("ALTER TABLE {} ADD COLUMN {}", table_sql(self, table)?, self.column_definition(column)?)]
            }
            Change::DropColumn { table, column } => {
                vec![format!("ALTER TABLE {} DROP COLUMN {}", table_sql(self, table)?, ident(self, "column", column)?)]
            }
            Change::RenameColumn { table, from, to } => vec![format!(
                "ALTER TABLE {} RENAME COLUMN {} TO {}",
                table_sql(self, table)?,
                ident(self, "column", from)?,
                ident(self, "column", to)?
            )],
            Change::AlterColumn { table, from, to } => {
                let t = table_sql(self, table)?;
                let column = ident(self, "column", &to.name)?;
                let mut statements = Vec::new();
                let old_default = from.default.as_deref().map(str::trim).filter(|d| !d.is_empty());
                let new_default = to.default.as_deref().map(str::trim).filter(|d| !d.is_empty());
                let old_type = column_type(&from.data_type, from.nullable);
                let type_text = type_operand(self, &from.data_type, &to.data_type)?;
                let new_type = column_type(&type_text, to.nullable);
                if from.generated && (old_type != new_type || old_default != new_default) {
                    return Err(DdlError::Unsupported(format!(
                        "{table}.{} is a MATERIALIZED / ALIAS column: its type and default are part of its expression and cannot be edited here.",
                        from.name
                    )));
                }
                if from.nullable && !to.nullable && new_default.is_none() {
                    return Err(DdlError::Invalid(format!(
                        "ClickHouse needs a default to make the Nullable column {} NOT NULL (rows that hold NULL take it).",
                        to.name
                    )));
                }
                if old_default.is_some() && new_default.is_none() {
                    statements.push(format!("ALTER TABLE {t} MODIFY COLUMN {column} REMOVE DEFAULT"));
                }
                if old_type != new_type || (new_default.is_some() && old_default != new_default) {
                    let default = match default_operand(self, from.default.as_deref(), to.default.as_deref())? {
                        Some(default) => format!(" DEFAULT {default}"),
                        None => String::new(),
                    };
                    statements.push(format!("ALTER TABLE {t} MODIFY COLUMN {column} {new_type}{default}"));
                }
                if from.comment.as_deref().unwrap_or("") != to.comment.as_deref().unwrap_or("") {
                    statements.push(format!(
                        "ALTER TABLE {t} COMMENT COLUMN {column} {}",
                        literal(self, to.comment.as_deref().unwrap_or(""))
                    ));
                }
                statements
            }
            Change::SetPrimaryKey { table, .. } => {
                return Err(DdlError::Unsupported(format!(
                    "ClickHouse has no editable primary key: the sorting key of {table} is fixed when the table is created."
                )))
            }
            Change::CreateIndex { .. } | Change::DropIndex { .. } => return Err(Self::refuse("secondary indexes")),
            Change::AddForeignKey { .. } | Change::DropForeignKey { .. } => return Err(Self::refuse("foreign keys")),
            Change::Account(account) => return self.account_statements(account, crate::sql_account::Reveal::Redacted),
        })
    }
}
