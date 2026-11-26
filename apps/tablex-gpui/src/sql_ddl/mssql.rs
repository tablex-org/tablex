//! SQL Server (T-SQL) DDL. Transactional.
//!
//! Names are written `[like this]`; the statements are read with `"…"` and `[…]` as names
//! ([`SqlDialect::MSSQL`]). What `ALTER TABLE` does not do in one statement is either done in
//! several (a rename is `sp_rename`) or refused with a message:
//!
//! * the default of an existing column is a constraint with a generated name, which the editor does
//!   not know, so changing or dropping it is refused (adding a column with a default is fine);
//! * a column's comment is an extended property, not part of the column: comments are not edited;
//! * `ALTER COLUMN` restates the type and the nullability only, so an `IDENTITY` column keeps its
//!   identity and a computed column is refused.

use super::{
    column_list, default_sql, ident, index_columns, literal, named_fk, objects, table_sql, type_operand, type_sql, Change,
    ColumnSpec, DdlCapabilities, DdlDialect, DdlError, ForeignKeySpec, NameRules, NamespaceKind, ObjectCapabilities,
    TableId,
};
use crate::sql_table::SqlDialect;

pub(super) struct SqlServer;

/// `N'…'`: the arguments of `sp_rename` are `nvarchar`.
fn nliteral(d: &dyn DdlDialect, text: &str) -> String {
    format!("N{}", literal(d, text))
}

impl SqlServer {
    /// `[name] type [IDENTITY(1,1)] [NOT] NULL [DEFAULT x]`
    fn column_definition(&self, column: &ColumnSpec) -> Result<String, DdlError> {
        if column.generated {
            return Err(DdlError::Unsupported(format!(
                "{} is a computed column: its expression is part of its definition and cannot be restated here.",
                column.name
            )));
        }
        let mut sql = format!("{} {}", ident(self, "column", &column.name)?, type_sql(self, &column.data_type)?);
        if column.auto_increment {
            sql.push_str(" IDENTITY(1,1)");
        }
        sql.push_str(if column.nullable { " NULL" } else { " NOT NULL" });
        if let Some(default) = column.default.as_deref().filter(|d| !d.trim().is_empty()) {
            sql.push_str(&format!(" DEFAULT {}", default_sql(self, default)?));
        }
        Ok(sql)
    }

    /// `[index] ON [schema].[table]` of an index.
    fn index_on(&self, table: &TableId, name: &str) -> Result<String, DdlError> {
        Ok(format!("{} ON {}", ident(self, "index", name)?, table_sql(self, table)?))
    }
}

impl DdlDialect for SqlServer {
    fn flavor(&self) -> super::Flavor {
        super::Flavor::SqlServer
    }

    fn label(&self) -> &'static str {
        "SQL Server"
    }

    fn quoting(&self) -> SqlDialect {
        SqlDialect::MSSQL
    }

    /// `[name]`, with `]` doubled.
    fn quote(&self, name: &str) -> String {
        let mut out = String::with_capacity(name.len() + 2);
        out.push('[');
        for ch in name.chars() {
            if ch == ']' {
                out.push(']');
            }
            out.push(ch);
        }
        out.push(']');
        out
    }

    fn transactional(&self) -> bool {
        true
    }

    fn schema_is_database(&self) -> bool {
        false
    }

    fn max_identifier_length(&self) -> Option<usize> {
        Some(128)
    }

    /// The default collation compares names without regard to case.
    fn name_rules(&self) -> NameRules {
        NameRules::IGNORE_CASE
    }

    fn capabilities(&self) -> DdlCapabilities {
        DdlCapabilities { indexes: true, foreign_keys: true, primary_key_edit: true, column_comments: false, alter_column: true }
    }

    fn object_capabilities(&self) -> ObjectCapabilities {
        ObjectCapabilities {
            drop_cascade: false,
            truncate_statement: true,
            truncate_options: false,
            rename_view: true,
            materialized_views: false,
            create_namespace: &[NamespaceKind::Database, NamespaceKind::Schema],
        }
    }

    fn common_types(&self) -> &'static [&'static str] {
        &[
            "int",
            "bigint",
            "smallint",
            "tinyint",
            "bit",
            "decimal(18, 2)",
            "float",
            "real",
            "money",
            "nvarchar(255)",
            "nvarchar(max)",
            "varchar(255)",
            "datetime2",
            "date",
            "time",
            "datetimeoffset",
            "uniqueidentifier",
            "varbinary(max)",
            "xml",
        ]
    }

    fn session_setup(&self, lock_timeout_secs: u32) -> super::SessionSetup {
        super::SessionSetup { before: vec![format!("SET LOCK_TIMEOUT {}", lock_timeout_secs.saturating_mul(1000))], in_transaction: Vec::new() }
    }

    fn statements(&self, change: &Change, _inline: &[ForeignKeySpec]) -> Result<Vec<String>, DdlError> {
        Ok(match change {
            Change::CreateTable { table, columns, primary_key, comment } => {
                if comment.as_deref().is_some_and(|comment| !comment.is_empty()) {
                    return Err(DdlError::Unsupported("SQL Server table comments are extended properties, which are not edited here.".into()));
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
                vec![format!("CREATE TABLE {} (\n  {}\n)", table_sql(self, table)?, definitions.join(",\n  "))]
            }
            Change::DropTable { table, cascade } => vec![objects::drop_statement(self, "TABLE", table, *cascade)?],
            Change::DropView { table, materialized, cascade } => {
                vec![objects::drop_statement(self, objects::view_keyword(self, *materialized)?, table, *cascade)?]
            }
            Change::RenameTable { table, to } | Change::RenameView { table, to, .. } => {
                objects::checked_name(self, "table", to)?;
                vec![format!("EXEC sp_rename {}, {}", nliteral(self, &table_sql(self, table)?), nliteral(self, to))]
            }
            Change::TruncateTable { table, restart_identity, cascade } => {
                objects::truncate_statements(self, table, *restart_identity, *cascade)?
            }
            Change::CreateNamespace { kind, name } => objects::create_namespace(self, *kind, name)?,
            Change::AddColumn { table, column } => {
                vec![format!("ALTER TABLE {} ADD {}", table_sql(self, table)?, self.column_definition(column)?)]
            }
            Change::DropColumn { table, column } => {
                vec![format!("ALTER TABLE {} DROP COLUMN {}", table_sql(self, table)?, ident(self, "column", column)?)]
            }
            Change::RenameColumn { table, from, to } => {
                ident(self, "column", to)?;
                let old = format!("{}.{}", table_sql(self, table)?, ident(self, "column", from)?);
                vec![format!("EXEC sp_rename {}, {}, N'COLUMN'", nliteral(self, &old), nliteral(self, to))]
            }
            Change::AlterColumn { table, from, to } => {
                if from.generated {
                    return Err(DdlError::Unsupported(format!(
                        "{table}.{} is a computed column: its type is part of its expression and cannot be edited here.",
                        from.name
                    )));
                }
                let (old_default, new_default) = (
                    from.default.as_deref().map(str::trim).filter(|d| !d.is_empty()),
                    to.default.as_deref().map(str::trim).filter(|d| !d.is_empty()),
                );
                if old_default != new_default {
                    return Err(DdlError::Unsupported(format!(
                        "SQL Server keeps the default of {table}.{} as a constraint with a generated name; change it in SQL (DROP CONSTRAINT, then ADD DEFAULT ... FOR ...).",
                        from.name
                    )));
                }
                if from.comment.as_deref().unwrap_or("") != to.comment.as_deref().unwrap_or("") {
                    return Err(DdlError::Unsupported("SQL Server column comments are extended properties, which are not edited here.".into()));
                }
                if from.data_type.trim() == to.data_type.trim() && from.nullable == to.nullable {
                    Vec::new()
                } else {
                    vec![format!(
                        "ALTER TABLE {} ALTER COLUMN {} {} {}",
                        table_sql(self, table)?,
                        ident(self, "column", &to.name)?,
                        type_operand(self, &from.data_type, &to.data_type)?,
                        if to.nullable { "NULL" } else { "NOT NULL" }
                    )]
                }
            }
            Change::SetPrimaryKey { table, columns, previous } => {
                let mut statements = Vec::new();
                if let Some(previous) = previous.as_ref().filter(|previous| !previous.columns.is_empty() || previous.name.is_some()) {
                    let Some(name) = previous.name.as_deref() else {
                        return Err(DdlError::Unsupported(format!(
                            "The primary key of {table} has no name the catalog reported, so it cannot be dropped here."
                        )));
                    };
                    statements.push(format!("ALTER TABLE {} DROP CONSTRAINT {}", table_sql(self, table)?, ident(self, "constraint", name)?));
                }
                if !columns.is_empty() {
                    statements.push(format!("ALTER TABLE {} ADD PRIMARY KEY ({})", table_sql(self, table)?, column_list(self, "primary key", columns)?));
                }
                statements
            }
            Change::CreateIndex { table, index } => vec![format!(
                "CREATE {}INDEX {} ({})",
                if index.unique { "UNIQUE " } else { "" },
                self.index_on(table, &index.name)?,
                index_columns(self, index)?
            )],
            Change::DropIndex { table, name } => vec![format!("DROP INDEX {}", self.index_on(table, name)?)],
            Change::AddForeignKey { table, foreign_key } => {
                vec![format!("ALTER TABLE {} ADD {}", table_sql(self, table)?, named_fk(self, foreign_key, true)?)]
            }
            Change::DropForeignKey { table, name } => {
                vec![format!("ALTER TABLE {} DROP CONSTRAINT {}", table_sql(self, table)?, ident(self, "foreign key", name)?)]
            }
            Change::Account(account) => return self.account_statements(account, crate::sql_account::Reveal::Redacted),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use crate::connections::ConnectionKind;

    fn dialect() -> &'static dyn DdlDialect {
        dialect_for(ConnectionKind::Mssql).expect("SQL Server has a DDL dialect")
    }

    fn column(name: &str, data_type: &str, nullable: bool) -> ColumnSpec {
        ColumnSpec { name: name.into(), data_type: data_type.into(), nullable, ..ColumnSpec::default() }
    }

    fn sql(change: Change) -> Vec<String> {
        dialect().statements(&change, &[]).expect("statements")
    }

    fn table() -> TableId {
        TableId::new(Some("dbo"), "orders")
    }

    #[test]
    fn names_are_bracketed_and_a_closing_bracket_is_doubled() {
        assert_eq!(dialect().quote("a]b"), "[a]]b]");
        assert_eq!(table_sql(dialect(), &TableId::new(Some("my]schema"), "t")).unwrap(), "[my]]schema].[t]");
        assert_eq!(dialect().quoting(), crate::sql_table::SqlDialect::MSSQL);
    }

    #[test]
    fn create_table_has_identity_nullability_and_a_primary_key() {
        let mut id = column("id", "int", false);
        id.auto_increment = true;
        let mut note = column("note", "nvarchar(max)", true);
        note.default = Some("n/a".into());
        assert_eq!(
            sql(Change::CreateTable { table: table(), columns: vec![id, note], primary_key: vec!["id".into()], comment: None }),
            ["CREATE TABLE [dbo].[orders] (\n  [id] int IDENTITY(1,1) NOT NULL,\n  [note] nvarchar(max) NULL DEFAULT 'n/a',\n  PRIMARY KEY ([id])\n)"]
        );
        let commented = Change::CreateTable { table: table(), columns: vec![column("a", "int", true)], primary_key: vec![], comment: Some("x".into()) };
        assert!(dialect().statements(&commented, &[]).is_err(), "a comment is an extended property");
    }

    #[test]
    fn renames_go_through_sp_rename_with_string_literals() {
        assert_eq!(sql(Change::RenameTable { table: table(), to: "o2".into() }), ["EXEC sp_rename N'[dbo].[orders]', N'o2'"]);
        assert_eq!(
            sql(Change::RenameColumn { table: table(), from: "a".into(), to: "b'c".into() }),
            ["EXEC sp_rename N'[dbo].[orders].[a]', N'b''c', N'COLUMN'"]
        );
    }

    #[test]
    fn columns_indexes_keys_and_namespaces() {
        assert_eq!(sql(Change::AddColumn { table: table(), column: column("qty", "int", true) }), ["ALTER TABLE [dbo].[orders] ADD [qty] int NULL"]);
        assert_eq!(sql(Change::DropColumn { table: table(), column: "qty".into() }), ["ALTER TABLE [dbo].[orders] DROP COLUMN [qty]"]);
        assert_eq!(
            sql(Change::AlterColumn { table: table(), from: column("qty", "int", true), to: column("qty", "bigint", false) }),
            ["ALTER TABLE [dbo].[orders] ALTER COLUMN [qty] bigint NOT NULL"]
        );
        assert_eq!(
            sql(Change::CreateIndex { table: table(), index: IndexSpec { name: "ix".into(), columns: vec!["a".into(), "b".into()], unique: true } }),
            ["CREATE UNIQUE INDEX [ix] ON [dbo].[orders] ([a], [b])"]
        );
        assert_eq!(sql(Change::DropIndex { table: table(), name: "ix".into() }), ["DROP INDEX [ix] ON [dbo].[orders]"]);
        assert_eq!(sql(Change::DropForeignKey { table: table(), name: "fk".into() }), ["ALTER TABLE [dbo].[orders] DROP CONSTRAINT [fk]"]);
        assert_eq!(
            sql(Change::SetPrimaryKey { table: table(), columns: vec!["id".into()], previous: Some(PrimaryKeyRef { name: Some("pk_o".into()), columns: vec!["a".into()] }) }),
            ["ALTER TABLE [dbo].[orders] DROP CONSTRAINT [pk_o]", "ALTER TABLE [dbo].[orders] ADD PRIMARY KEY ([id])"]
        );
        assert_eq!(sql(Change::CreateNamespace { kind: NamespaceKind::Schema, name: "sales".into() }), ["CREATE SCHEMA [sales]"]);
    }

    #[test]
    fn what_needs_a_constraint_name_is_refused_not_guessed() {
        let mut with_default = column("qty", "int", true);
        with_default.default = Some("0".into());
        let change = Change::AlterColumn { table: table(), from: column("qty", "int", true), to: with_default };
        let error = dialect().statements(&change, &[]).unwrap_err();
        assert!(error.to_string().contains("constraint"), "{error}");
        let unnamed = Change::SetPrimaryKey { table: table(), columns: vec![], previous: Some(PrimaryKeyRef { name: None, columns: vec!["a".into()] }) };
        assert!(dialect().statements(&unnamed, &[]).is_err());
    }

    #[test]
    fn the_dialect_describes_itself() {
        let d = dialect();
        assert!(d.transactional() && !d.schema_is_database());
        assert_eq!(d.max_identifier_length(), Some(128));
        // tinyint is unsigned: it widens to smallint, never the other way.
        assert!(widens(Flavor::SqlServer, "tinyint", "smallint"));
        assert!(!widens(Flavor::SqlServer, "smallint", "tinyint"));
        assert!(widens(Flavor::SqlServer, "datetime2(3)", "datetime2(7)"));
        assert!(widens(Flavor::SqlServer, "varchar(10)", "varchar(max)"));
    }
}
