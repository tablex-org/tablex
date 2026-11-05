//! SQLite DDL. Transactional.
//!
//! `ALTER TABLE` can add, rename and drop a column and rename a table, nothing
//! else: changing a column's type, nullability or default, the primary key, or
//! adding / dropping a foreign key of an existing table needs the table rebuilt
//! (create a copy, copy the rows, drop, rename). That is refused with a message
//! rather than done: a rebuild must carry every trigger, index, view and
//! constraint of the table along, which the diagram does not know. Foreign keys
//! of a table created in the same change-set go inside its `CREATE TABLE`.

use super::{
    column_list, default_sql, ident, index_columns, named_fk, objects, table_sql, type_sql, Change, ColumnSpec,
    DdlCapabilities, DdlDialect, NameRules, DdlError, ForeignKeySpec, ObjectCapabilities, TableId,
};
use crate::sql_table::SqlDialect;

pub(super) struct Sqlite;

const REBUILD: &str = "without rebuilding the table";

impl Sqlite {
    fn column_definition(&self, column: &ColumnSpec) -> Result<String, DdlError> {
        if column.comment.as_deref().is_some_and(|comment| !comment.is_empty()) {
            return Err(DdlError::Unsupported(format!("SQLite has no column comments ({}).", column.name)));
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

impl DdlDialect for Sqlite {
    fn flavor(&self) -> super::Flavor {
        super::Flavor::Sqlite
    }

    fn label(&self) -> &'static str {
        "SQLite"
    }

    fn quoting(&self) -> SqlDialect {
        SqlDialect::SQLITE
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
            drop_cascade: false,
            truncate_statement: false,
            truncate_options: false,
            rename_view: false,
            materialized_views: false,
            create_namespace: &[],
        }
    }

    fn common_types(&self) -> &'static [&'static str] {
        &["INTEGER", "TEXT", "REAL", "NUMERIC", "BLOB", "BOOLEAN", "DATETIME"]
    }

    fn inline_foreign_keys(&self) -> bool {
        true
    }

    fn session_setup(&self, lock_timeout_secs: u32) -> super::SessionSetup {
        super::SessionSetup { before: vec![format!("PRAGMA busy_timeout = {}", lock_timeout_secs * 1000)], in_transaction: Vec::new() }
    }

    fn statements(&self, change: &Change, inline: &[ForeignKeySpec]) -> Result<Vec<String>, DdlError> {
        Ok(match change {
            Change::CreateTable { table, columns, primary_key, comment } => {
                if comment.as_deref().is_some_and(|comment| !comment.is_empty()) {
                    return Err(DdlError::Unsupported("SQLite has no table comments.".into()));
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
                    // The parent in a REFERENCES clause is never schema-qualified.
                    let mut foreign_key = foreign_key.clone();
                    foreign_key.ref_table.schema = None;
                    definitions.push(named_fk(self, &foreign_key, true)?);
                }
                vec![format!("CREATE TABLE {} (\n  {}\n)", table_sql(self, table)?, definitions.join(",\n  "))]
            }
            Change::DropTable { table, cascade } => vec![objects::drop_statement(self, "TABLE", table, *cascade)?],
            Change::DropView { table, materialized, cascade } => {
                vec![objects::drop_statement(self, objects::view_keyword(self, *materialized)?, table, *cascade)?]
            }
            Change::RenameView { table, .. } => {
                return Err(DdlError::Unsupported(format!(
                    "SQLite cannot rename the view {table}; create it again under the new name and drop the old one."
                )))
            }
            Change::TruncateTable { table, restart_identity, cascade } => {
                objects::truncate_statements(self, table, *restart_identity, *cascade)?
            }
            Change::CreateNamespace { kind, name } => objects::create_namespace(self, *kind, name)?,
            Change::RenameTable { table, to } => {
                vec![format!("ALTER TABLE {} RENAME TO {}", table_sql(self, table)?, ident(self, "table", to)?)]
            }
            Change::AddColumn { table, column } => {
                if !column.nullable && column.default.as_deref().is_none_or(|d| d.trim().is_empty()) {
                    return Err(DdlError::Unsupported(format!(
                        "SQLite cannot add the NOT NULL column {} without a default {REBUILD}; give it a default or allow NULL.",
                        column.name
                    )));
                }
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
                    "SQLite cannot change the type, nullability, default or comment of the existing column {} {REBUILD}.",
                    from.name
                )))
            }
            Change::SetPrimaryKey { table, .. } => {
                return Err(DdlError::Unsupported(format!("SQLite cannot change the primary key of {table} {REBUILD}.")))
            }
            Change::CreateIndex { table, index } => vec![format!(
                "CREATE {}INDEX {} ON {} ({})",
                if index.unique { "UNIQUE " } else { "" },
                self.index_name(table, &index.name)?,
                ident(self, "table", &table.name)?,
                index_columns(self, index)?
            )],
            Change::DropIndex { table, name } => vec![format!("DROP INDEX {}", self.index_name(table, name)?)],
            Change::AddForeignKey { table, foreign_key } => {
                return Err(DdlError::Unsupported(format!(
                    "SQLite cannot add the foreign key {} to the existing table {table} {REBUILD}.",
                    foreign_key.name
                )))
            }
            Change::DropForeignKey { table, name } => {
                return Err(DdlError::Unsupported(format!(
                    "SQLite cannot drop the foreign key {name} of {table} {REBUILD}."
                )))
            }
            Change::Account(account) => return self.account_statements(account, crate::sql_account::Reveal::Redacted),
        })
    }
}
