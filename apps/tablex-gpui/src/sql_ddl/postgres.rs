//! PostgreSQL DDL. Transactional; comments are `COMMENT ON COLUMN`.

use super::{
    column_list, default_operand, default_sql, ident, index_columns, literal, named_fk, table_sql, type_operand, type_sql,
    objects, Change, ColumnSpec, DdlCapabilities, DdlDialect, DdlError, ForeignKeySpec, NamespaceKind, ObjectCapabilities, SessionSetup,
    TableId,
};
use crate::sql_account::{AccountChange, AccountEngine, Reveal};
use crate::sql_table::SqlDialect;

pub(super) struct Postgres;

impl Postgres {
    /// `"name" type [NOT NULL] [DEFAULT x]`
    fn column_definition(&self, column: &ColumnSpec) -> Result<String, DdlError> {
        let mut sql = format!("{} {}", ident(self, "column", &column.name)?, type_sql(self, &column.data_type)?);
        if !column.nullable {
            sql.push_str(" NOT NULL");
        }
        if let Some(default) = column.default.as_deref().filter(|d| !d.trim().is_empty()) {
            sql.push_str(&format!(" DEFAULT {}", default_sql(self, default)?));
        }
        Ok(sql)
    }

    fn comment(&self, table: &TableId, column: &str, comment: Option<&str>) -> Result<String, DdlError> {
        Ok(format!(
            "COMMENT ON COLUMN {}.{} IS {}",
            table_sql(self, table)?,
            ident(self, "column", column)?,
            match comment.filter(|comment| !comment.is_empty()) {
                Some(comment) => literal(self, comment),
                None => "NULL".to_owned(),
            }
        ))
    }

    fn alter_column(&self, table: &TableId, from: &ColumnSpec, to: &ColumnSpec) -> Result<Vec<String>, DdlError> {
        let (t, column) = (table_sql(self, table)?, ident(self, "column", &to.name)?);
        let mut statements = Vec::new();
        let default_changed = from.default.as_deref().map(str::trim).filter(|d| !d.is_empty())
            != to.default.as_deref().map(str::trim).filter(|d| !d.is_empty());
        // An unchanged default is the catalog's own expression: restored as printed.
        let new_default = default_operand(self, from.default.as_deref(), to.default.as_deref())?;
        if from.data_type.trim() != to.data_type.trim() {
            let data_type = type_operand(self, &from.data_type, &to.data_type)?;
            // The old default may not cast to the new type, so it is dropped and
            // put back (exactly as the catalog printed it) inside the same
            // statement: it fails atomically. Without a type change it is not touched.
            let mut actions = Vec::new();
            if from.default.as_deref().is_some_and(|d| !d.trim().is_empty()) {
                actions.push(format!("ALTER COLUMN {column} DROP DEFAULT"));
            }
            actions.push(format!("ALTER COLUMN {column} TYPE {data_type} USING {column}::{data_type}"));
            if let Some(default) = &new_default {
                actions.push(format!("ALTER COLUMN {column} SET DEFAULT {default}"));
            }
            statements.push(format!("ALTER TABLE {t} {}", actions.join(", ")));
        } else if default_changed {
            statements.push(match &new_default {
                Some(default) => format!("ALTER TABLE {t} ALTER COLUMN {column} SET DEFAULT {default}"),
                None => format!("ALTER TABLE {t} ALTER COLUMN {column} DROP DEFAULT"),
            });
        }
        if from.nullable != to.nullable {
            statements.push(format!(
                "ALTER TABLE {t} ALTER COLUMN {column} {} NOT NULL",
                if to.nullable { "DROP" } else { "SET" }
            ));
        }
        if from.comment.as_deref().unwrap_or("") != to.comment.as_deref().unwrap_or("") {
            statements.push(self.comment(table, &to.name, to.comment.as_deref())?);
        }
        Ok(statements)
    }
}

impl DdlDialect for Postgres {
    fn flavor(&self) -> super::Flavor {
        super::Flavor::Postgres
    }

    fn label(&self) -> &'static str {
        "PostgreSQL"
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
        // Longer names are silently truncated by the server.
        Some(63)
    }

    fn identifier_length(&self, name: &str) -> usize {
        name.len()
    }

    fn length_unit(&self) -> &'static str {
        "bytes"
    }

    fn capabilities(&self) -> DdlCapabilities {
        DdlCapabilities { indexes: true, foreign_keys: true, primary_key_edit: true, column_comments: true, alter_column: true }
    }

    fn object_capabilities(&self) -> ObjectCapabilities {
        ObjectCapabilities {
            drop_cascade: true,
            truncate_statement: true,
            truncate_options: true,
            rename_view: true,
            materialized_views: true,
            create_namespace: &[NamespaceKind::Schema, NamespaceKind::Database],
        }
    }

    fn common_types(&self) -> &'static [&'static str] {
        &[
            "integer", "bigint", "smallint", "serial", "bigserial", "numeric(12,2)", "real", "double precision",
            "boolean", "text", "varchar(255)", "char(1)", "date", "time", "timestamp", "timestamptz", "uuid", "jsonb",
            "json", "bytea",
        ]
    }

    fn session_setup(&self, lock_timeout_secs: u32) -> SessionSetup {
        // `SET LOCAL` ends with the transaction: the settings never outlive the batch.
        SessionSetup {
            before: Vec::new(),
            in_transaction: vec![
                format!("SET LOCAL lock_timeout = '{lock_timeout_secs}s'"),
                "SET LOCAL statement_timeout = 0".to_owned(),
                // Comments and defaults are string literals written for the standard rules (a backslash
                // is a plain character); a server or role that has this off would read `\'` as an escaped
                // quote and the rest of the literal as SQL.
                "SET LOCAL standard_conforming_strings = on".to_owned(),
            ],
        }
    }

    fn check_locks(&self, tables: &[TableId]) -> Result<Vec<String>, DdlError> {
        tables.iter().map(|table| Ok(format!("LOCK TABLE {} IN SHARE UPDATE EXCLUSIVE MODE", table_sql(self, table)?))).collect()
    }

    fn statements(&self, change: &Change, _inline: &[ForeignKeySpec]) -> Result<Vec<String>, DdlError> {
        Ok(match change {
            Change::CreateTable { table, columns, primary_key, comment } => {
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
                let mut statements =
                    vec![format!("CREATE TABLE {} (\n  {}\n)", table_sql(self, table)?, definitions.join(",\n  "))];
                for column in columns.iter().filter(|column| column.comment.as_deref().is_some_and(|c| !c.is_empty())) {
                    statements.push(self.comment(table, &column.name, column.comment.as_deref())?);
                }
                if let Some(comment) = comment.as_deref().filter(|comment| !comment.is_empty()) {
                    statements.push(format!("COMMENT ON TABLE {} IS {}", table_sql(self, table)?, literal(self, comment)));
                }
                statements
            }
            Change::DropTable { table, cascade } => vec![objects::drop_statement(self, "TABLE", table, *cascade)?],
            Change::DropView { table, materialized, cascade } => {
                vec![objects::drop_statement(self, objects::view_keyword(self, *materialized)?, table, *cascade)?]
            }
            Change::RenameTable { table, to } => {
                vec![format!("ALTER TABLE {} RENAME TO {}", table_sql(self, table)?, ident(self, "table", to)?)]
            }
            Change::RenameView { table, to, materialized } => vec![format!(
                "ALTER {} {} RENAME TO {}",
                objects::view_keyword(self, *materialized)?,
                table_sql(self, table)?,
                objects::checked_name(self, "view", to)?
            )],
            Change::TruncateTable { table, restart_identity, cascade } => {
                objects::truncate_statements(self, table, *restart_identity, *cascade)?
            }
            Change::CreateNamespace { kind, name } => objects::create_namespace(self, *kind, name)?,
            Change::AddColumn { table, column } => {
                let mut statements =
                    vec![format!("ALTER TABLE {} ADD COLUMN {}", table_sql(self, table)?, self.column_definition(column)?)];
                if column.comment.as_deref().is_some_and(|c| !c.is_empty()) {
                    statements.push(self.comment(table, &column.name, column.comment.as_deref())?);
                }
                statements
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
            Change::AlterColumn { table, from, to } => self.alter_column(table, from, to)?,
            Change::SetPrimaryKey { table, columns, previous } => {
                let t = table_sql(self, table)?;
                let mut statements = Vec::new();
                if let Some(previous) = previous.as_ref().filter(|previous| !previous.columns.is_empty()) {
                    let Some(name) = previous.name.as_deref().filter(|name| !name.is_empty()) else {
                        return Err(DdlError::Invalid(format!(
                            "The name of the current primary key of {table} is unknown, so it cannot be dropped."
                        )));
                    };
                    statements.push(format!("ALTER TABLE {t} DROP CONSTRAINT {}", ident(self, "constraint", name)?));
                }
                if !columns.is_empty() {
                    statements.push(format!("ALTER TABLE {t} ADD PRIMARY KEY ({})", column_list(self, "primary key", columns)?));
                }
                statements
            }
            Change::CreateIndex { table, index } => vec![format!(
                "CREATE {}INDEX {} ON {} ({})",
                if index.unique { "UNIQUE " } else { "" },
                ident(self, "index", &index.name)?,
                table_sql(self, table)?,
                index_columns(self, index)?
            )],
            Change::DropIndex { table, name } => {
                // An index lives in its table's schema.
                let qualified = match table.schema.as_deref().filter(|schema| !schema.is_empty()) {
                    Some(schema) => format!("{}.{}", ident(self, "schema", schema)?, ident(self, "index", name)?),
                    None => ident(self, "index", name)?,
                };
                vec![format!("DROP INDEX {qualified}")]
            }
            Change::AddForeignKey { table, foreign_key } => {
                vec![format!("ALTER TABLE {} ADD {}", table_sql(self, table)?, named_fk(self, foreign_key, true)?)]
            }
            Change::DropForeignKey { table, name } => {
                vec![format!("ALTER TABLE {} DROP CONSTRAINT {}", table_sql(self, table)?, ident(self, "foreign key", name)?)]
            }
            Change::Account(account) => return self.account_statements(account, Reveal::Redacted),
        })
    }

    fn account_statements(&self, change: &AccountChange, reveal: Reveal) -> Result<Vec<String>, DdlError> {
        crate::sql_account::postgres_statements(self, change, reveal)
    }

    fn account_engine(&self) -> Option<AccountEngine> {
        Some(AccountEngine::Postgres)
    }
}
