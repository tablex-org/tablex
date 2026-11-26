//! DuckDB DDL. Transactional.
//!
//! The editor creates, renames and drops tables, columns and indexes. It does not change an existing
//! column, the primary key of a table or its foreign keys: DuckDB's `ALTER TABLE` has them with
//! limits (a column an index uses cannot be altered, a foreign key cannot be added to a table that
//! exists), and a refusal that names the limit beats a statement that fails half way. A foreign key
//! of a table created in the same change-set goes inside its `CREATE TABLE`, without referential
//! actions: DuckDB has none (`ON DELETE CASCADE` is an error there).

use super::{
    column_list, default_sql, ident, index_columns, named_fk, objects, table_sql, type_sql, Change, ColumnSpec,
    DdlCapabilities, DdlDialect, DdlError, FkAction, ForeignKeySpec, NameRules, NamespaceKind, ObjectCapabilities, TableId,
};
use crate::sql_table::SqlDialect;

pub(super) struct DuckDb;

const EXISTING: &str = "of an existing table here; run ALTER TABLE in a SQL tab";

impl DuckDb {
    fn column_definition(&self, column: &ColumnSpec) -> Result<String, DdlError> {
        if column.comment.as_deref().is_some_and(|comment| !comment.is_empty()) {
            return Err(DdlError::Unsupported(format!(
                "The editor does not write column comments for DuckDB ({}); use COMMENT ON COLUMN in a SQL tab.",
                column.name
            )));
        }
        let mut sql = format!("{} {}", ident(self, "column", &column.name)?, type_sql(self, &column.data_type)?);
        if !column.nullable {
            sql.push_str(" NOT NULL");
        }
        if let Some(default) = column.default.as_deref().filter(|d| !d.trim().is_empty()) {
            sql.push_str(&format!(" DEFAULT {}", default_sql(self, default)?));
        }
        Ok(sql)
    }

    /// `"schema"."name"` of an index (an index lives in its table's schema).
    fn index_name(&self, table: &TableId, name: &str) -> Result<String, DdlError> {
        let name = ident(self, "index", name)?;
        Ok(match table.schema.as_deref().filter(|schema| !schema.is_empty()) {
            Some(schema) => format!("{}.{name}", ident(self, "schema", schema)?),
            None => name,
        })
    }
}

impl DdlDialect for DuckDb {
    fn flavor(&self) -> super::Flavor {
        // No existing column is altered, so no type change is ever classified; its types read as
        // PostgreSQL's (unbounded `varchar`, microsecond `timestamp`).
        super::Flavor::Postgres
    }

    fn label(&self) -> &'static str {
        "DuckDB"
    }

    fn quoting(&self) -> SqlDialect {
        SqlDialect::POSTGRES
    }

    fn transactional(&self) -> bool {
        true
    }

    fn schema_is_database(&self) -> bool {
        false
    }

    fn max_identifier_length(&self) -> Option<usize> {
        None
    }

    fn name_rules(&self) -> NameRules {
        NameRules::IGNORE_CASE
    }

    fn capabilities(&self) -> DdlCapabilities {
        DdlCapabilities {
            indexes: true,
            // Only on a table created in the same change-set.
            foreign_keys: false,
            primary_key_edit: false,
            column_comments: false,
            alter_column: false,
        }
    }

    fn object_capabilities(&self) -> ObjectCapabilities {
        ObjectCapabilities {
            drop_cascade: true,
            truncate_statement: true,
            truncate_options: false,
            rename_view: true,
            materialized_views: false,
            create_namespace: &[NamespaceKind::Schema],
        }
    }

    fn common_types(&self) -> &'static [&'static str] {
        &[
            "INTEGER", "BIGINT", "HUGEINT", "DOUBLE", "DECIMAL(18,3)", "VARCHAR", "BOOLEAN", "DATE", "TIMESTAMP",
            "TIMESTAMPTZ", "BLOB", "UUID",
        ]
    }

    fn inline_foreign_keys(&self) -> bool {
        true
    }

    fn statements(&self, change: &Change, inline: &[ForeignKeySpec]) -> Result<Vec<String>, DdlError> {
        Ok(match change {
            Change::CreateTable { table, columns, primary_key, comment } => {
                if comment.as_deref().is_some_and(|comment| !comment.is_empty()) {
                    return Err(DdlError::Unsupported(
                        "The editor does not write table comments for DuckDB; use COMMENT ON TABLE in a SQL tab.".into(),
                    ));
                }
                if columns.is_empty() {
                    return Err(DdlError::Invalid(format!("Table {table} has no column.")));
                }
                let mut definitions = columns
                    .iter()
                    .map(|column| {
                        let mut column = column.clone();
                        column.nullable &= !primary_key.contains(&column.name);
                        self.column_definition(&column)
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                if !primary_key.is_empty() {
                    definitions.push(format!("PRIMARY KEY ({})", column_list(self, "primary key", primary_key)?));
                }
                for foreign_key in inline {
                    let actions = [foreign_key.on_delete, foreign_key.on_update];
                    if actions.iter().flatten().any(|action| *action != FkAction::NoAction) {
                        return Err(DdlError::Unsupported(format!(
                            "DuckDB has no referential actions: the foreign key {} cannot ON DELETE / ON UPDATE anything but NO ACTION.",
                            foreign_key.name
                        )));
                    }
                    definitions.push(named_fk(self, foreign_key, false)?);
                }
                vec![format!("CREATE TABLE {} (\n  {}\n)", table_sql(self, table)?, definitions.join(",\n  "))]
            }
            Change::DropTable { table, cascade } => vec![objects::drop_statement(self, "TABLE", table, *cascade)?],
            Change::DropView { table, materialized, cascade } => {
                vec![objects::drop_statement(self, objects::view_keyword(self, *materialized)?, table, *cascade)?]
            }
            Change::RenameView { table, to, materialized } => {
                objects::view_keyword(self, *materialized)?;
                vec![format!("ALTER VIEW {} RENAME TO {}", table_sql(self, table)?, ident(self, "view", to)?)]
            }
            Change::TruncateTable { table, restart_identity, cascade } => {
                objects::truncate_statements(self, table, *restart_identity, *cascade)?
            }
            Change::CreateNamespace { kind, name } => objects::create_namespace(self, *kind, name)?,
            Change::RenameTable { table, to } => {
                vec![format!("ALTER TABLE {} RENAME TO {}", table_sql(self, table)?, ident(self, "table", to)?)]
            }
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
            Change::AlterColumn { from, .. } => {
                return Err(DdlError::Unsupported(format!(
                    "The editor cannot change the type, nullability or default of the column {} {EXISTING}.",
                    from.name
                )))
            }
            Change::SetPrimaryKey { table, .. } => {
                return Err(DdlError::Unsupported(format!("The editor cannot change the primary key of {table} {EXISTING}.")))
            }
            Change::CreateIndex { table, index } => vec![format!(
                "CREATE {}INDEX {} ON {} ({})",
                if index.unique { "UNIQUE " } else { "" },
                ident(self, "index", &index.name)?,
                table_sql(self, table)?,
                index_columns(self, index)?
            )],
            Change::DropIndex { table, name } => vec![format!("DROP INDEX {}", self.index_name(table, name)?)],
            Change::AddForeignKey { table, foreign_key } => {
                return Err(DdlError::Unsupported(format!(
                    "DuckDB cannot add the foreign key {} to the existing table {table}: a foreign key goes in the CREATE TABLE.",
                    foreign_key.name
                )))
            }
            Change::DropForeignKey { table, name } => {
                return Err(DdlError::Unsupported(format!(
                    "DuckDB cannot drop the foreign key {name} of {table}: the table would have to be created again."
                )))
            }
            Change::Account(account) => return self.account_statements(account, crate::sql_account::Reveal::Redacted),
        })
    }
}
