//! MySQL and MariaDB DDL. Not transactional (every DDL statement commits).
//!
//! `RENAME COLUMN` needs MySQL 8.0 / MariaDB 10.5.2. `MODIFY COLUMN` restates
//! the whole column, so it needs the whole column: type, character set and
//! collation, nullability, default, `ON UPDATE`, `AUTO_INCREMENT`, `INVISIBLE`,
//! the generation expression and the comment. The driver reports them
//! (`ColumnSpec::attributes`); a column for which any is unknown (an older
//! driver, an `EXTRA` token this code does not know) is refused instead of
//! restated from a partial picture.

use super::{
    column_list, default_operand, default_sql, ident, index_columns, literal, named_fk, table_sql, type_operand, type_sql,
    objects, Change, ColumnSpec, DdlCapabilities, DdlDialect, NameRules, DdlError, ForeignKeySpec, NamespaceKind, ObjectCapabilities,
    SessionSetup,
};
use crate::sql_account::{AccountChange, AccountEngine, Reveal};
use crate::sql_table::SqlDialect;

pub(super) struct MySql {
    pub(super) mariadb: bool,
    /// `sql_mode` has `NO_BACKSLASH_ESCAPES`: a backslash in a string is an ordinary character.
    pub(super) no_backslash_escapes: bool,
    /// `sql_mode` has `ANSI_QUOTES` (or `ANSI`, MariaDB `MSSQL`): `"…"` quotes a name.
    pub(super) ansi_quotes: bool,
    /// MariaDB `sql_mode=MSSQL`: `[name]` quotes a name.
    pub(super) bracket_identifiers: bool,
}

/// What `EXTRA` says about a column.
#[derive(Debug, Default, PartialEq, Eq)]
struct Extra {
    auto_increment: bool,
    invisible: bool,
    /// `CURRENT_TIMESTAMP` or `CURRENT_TIMESTAMP(n)`.
    on_update: Option<String>,
    /// `Some(stored)` for a generated column.
    generated: Option<bool>,
}

/// Reads the raw `EXTRA` text; `Err` names a token that is not understood.
fn parse_extra(extra: &str) -> Result<Extra, String> {
    let mut parsed = Extra::default();
    let tokens: Vec<String> = extra.to_ascii_lowercase().split_whitespace().map(str::to_owned).collect();
    let mut at = 0;
    while at < tokens.len() {
        let token = tokens[at].as_str();
        match token {
            "auto_increment" => parsed.auto_increment = true,
            "default_generated" | "generated" => {}
            "invisible" => parsed.invisible = true,
            "virtual" | "stored" | "persistent" => {
                parsed.generated = Some(token != "virtual");
                if tokens.get(at + 1).map(String::as_str) == Some("generated") {
                    at += 1;
                }
            }
            "on" if tokens.get(at + 1).map(String::as_str) == Some("update") => {
                let Some(function) = tokens.get(at + 2).and_then(|t| t.strip_prefix("current_timestamp")) else {
                    return Err(extra.to_owned());
                };
                let digits = function.trim_start_matches('(').trim_end_matches(')');
                if !digits.chars().all(|c| c.is_ascii_digit()) {
                    return Err(extra.to_owned());
                }
                parsed.on_update = Some(if digits.is_empty() { "CURRENT_TIMESTAMP".into() } else { format!("CURRENT_TIMESTAMP({digits})") });
                at += 2;
            }
            _ => return Err(extra.to_owned()),
        }
        at += 1;
    }
    Ok(parsed)
}

/// Types that carry a character set.
fn is_string_type(data_type: &str) -> bool {
    let base = data_type.trim().to_ascii_lowercase();
    let base = base.split(|c: char| c == '(' || c.is_whitespace()).next().unwrap_or("");
    matches!(base, "char" | "varchar" | "tinytext" | "text" | "mediumtext" | "longtext" | "enum" | "set")
}

fn plain_name(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

impl MySql {
    /// `` `name` type NULL|NOT NULL [DEFAULT x] [AUTO_INCREMENT] [COMMENT 'x'] ``
    fn column_definition(&self, column: &ColumnSpec) -> Result<String, DdlError> {
        let mut sql = format!("{} {}", ident(self, "column", &column.name)?, type_sql(self, &column.data_type)?);
        sql.push_str(if column.nullable { " NULL" } else { " NOT NULL" });
        if let Some(default) = column.default.as_deref().filter(|d| !d.trim().is_empty()) {
            sql.push_str(&format!(" DEFAULT {}", default_sql(self, default)?));
        }
        if column.auto_increment {
            sql.push_str(" AUTO_INCREMENT");
        }
        if let Some(comment) = column.comment.as_deref().filter(|comment| !comment.is_empty()) {
            sql.push_str(&format!(" COMMENT {}", literal(self, comment)));
        }
        Ok(sql)
    }
}

impl MySql {
    /// The full definition of an existing column after the edit: the new type,
    /// nullability, default and comment, and every attribute of the old column
    /// restated. Refuses when an attribute is unknown.
    fn restated_column(&self, table: &super::TableId, from: &ColumnSpec, to: &ColumnSpec) -> Result<String, DdlError> {
        let unknown = |what: &str| {
            DdlError::Unsupported(format!(
                "{table}.{} cannot be restated safely: {what}. Update the {} driver, or change the column in SQL.",
                from.name,
                self.label()
            ))
        };
        let Some(attributes) = from.attributes.as_ref() else {
            return Err(unknown("the driver did not report its character set, collation and extra attributes"));
        };
        let extra = parse_extra(&attributes.extra).map_err(|text| unknown(&format!("its EXTRA \"{text}\" is not understood")))?;
        // The catalog prints its text with backslashes escaped (`'a\\b'` for `a\b`): restated verbatim under
        // NO_BACKSLASH_ESCAPES the server would read two backslashes, so such text is never restated in that mode.
        let backslashes = |what: &str, text: &str| {
            (self.no_backslash_escapes && text.contains('\\'))
                .then(|| unknown(&format!("its {what} contains a backslash, which the catalog prints escaped and this server's sql_mode (NO_BACKSLASH_ESCAPES) reads literally")))
        };
        if from.data_type.trim() == to.data_type.trim() {
            if let Some(error) = backslashes("type", &from.data_type) {
                return Err(error);
            }
        }
        let data_type = type_operand(self, &from.data_type, &to.data_type)?;
        let mut sql = format!("{} {data_type}", ident(self, "column", &to.name)?);
        if is_string_type(&to.data_type) {
            let (Some(set), Some(collation)) = (attributes.character_set.as_deref(), attributes.collation.as_deref()) else {
                return Err(unknown("the driver did not report its character set and collation"));
            };
            if !plain_name(set) || !plain_name(collation) {
                return Err(unknown("its character set or collation has an unusual name"));
            }
            sql.push_str(&format!(" CHARACTER SET {set} COLLATE {collation}"));
        }
        match (&extra.generated, attributes.generation_expression.as_deref().filter(|e| !e.is_empty())) {
            (Some(stored), Some(expression)) => {
                if let Some(error) = backslashes("generation expression", expression) {
                    return Err(error);
                }
                sql.push_str(&format!(" GENERATED ALWAYS AS ({expression}) {}", if *stored { "STORED" } else { "VIRTUAL" }));
            }
            (Some(_), None) => return Err(unknown("it is generated and its expression is not known")),
            (None, _) => {}
        }
        sql.push_str(if to.nullable { " NULL" } else { " NOT NULL" });
        if extra.generated.is_none() {
            let unchanged = from.default.as_deref().map(str::trim) == to.default.as_deref().map(str::trim);
            if let (true, Some(default)) = (unchanged, from.default.as_deref()) {
                if let Some(error) = backslashes("default", default) {
                    return Err(error);
                }
            }
            if let Some(default) = default_operand(self, from.default.as_deref(), to.default.as_deref())? {
                sql.push_str(&format!(" DEFAULT {default}"));
            }
        }
        if let Some(on_update) = &extra.on_update {
            sql.push_str(&format!(" ON UPDATE {on_update}"));
        }
        if extra.invisible {
            sql.push_str(" INVISIBLE");
        }
        if extra.auto_increment {
            sql.push_str(" AUTO_INCREMENT");
        }
        if let Some(comment) = to.comment.as_deref().filter(|comment| !comment.is_empty()) {
            sql.push_str(&format!(" COMMENT {}", literal(self, comment)));
        }
        Ok(sql)
    }
}

impl DdlDialect for MySql {
    fn flavor(&self) -> super::Flavor {
        super::Flavor::MySql
    }

    fn label(&self) -> &'static str {
        if self.mariadb {
            "MariaDB"
        } else {
            "MySQL"
        }
    }

    fn quoting(&self) -> SqlDialect {
        SqlDialect {
            backslash_escapes: !self.no_backslash_escapes,
            double_quote_identifiers: self.ansi_quotes,
            bracket_identifiers: self.bracket_identifiers,
            ..SqlDialect::MYSQL
        }
    }

    fn session_setup(&self, lock_timeout_secs: u32) -> SessionSetup {
        SessionSetup { before: vec![format!("SET SESSION lock_wait_timeout = {lock_timeout_secs}")], in_transaction: Vec::new() }
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
        Some(64)
    }

    fn name_rules(&self) -> NameRules {
        NameRules::IGNORE_CASE
    }

    fn capabilities(&self) -> DdlCapabilities {
        DdlCapabilities { indexes: true, foreign_keys: true, primary_key_edit: true, column_comments: true, alter_column: true }
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
        &[
            "int", "bigint", "smallint", "tinyint(1)", "decimal(12,2)", "float", "double", "varchar(255)", "char(1)",
            "text", "longtext", "date", "datetime", "timestamp", "time", "json", "blob",
        ]
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
                let comment = match comment.as_deref().filter(|comment| !comment.is_empty()) {
                    Some(comment) => format!(" COMMENT = {}", literal(self, comment)),
                    None => String::new(),
                };
                vec![format!("CREATE TABLE {} (\n  {}\n){comment}", table_sql(self, table)?, definitions.join(",\n  "))]
            }
            Change::DropTable { table, cascade } => vec![objects::drop_statement(self, "TABLE", table, *cascade)?],
            Change::DropView { table, materialized, cascade } => {
                vec![objects::drop_statement(self, objects::view_keyword(self, *materialized)?, table, *cascade)?]
            }
            Change::RenameTable { table, to } => {
                let target = super::TableId { schema: table.schema.clone(), name: to.clone() };
                vec![format!("RENAME TABLE {} TO {}", table_sql(self, table)?, table_sql(self, &target)?)]
            }
            // `RENAME TABLE` renames a view as well, inside its database.
            Change::RenameView { table, to, materialized } => {
                objects::view_keyword(self, *materialized)?;
                objects::checked_name(self, "view", to)?;
                let target = super::TableId { schema: table.schema.clone(), name: to.clone() };
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
                let changed = from.data_type.trim() != to.data_type.trim()
                    || from.nullable != to.nullable
                    || from.default.as_deref().map(str::trim).filter(|d| !d.is_empty())
                        != to.default.as_deref().map(str::trim).filter(|d| !d.is_empty())
                    || from.comment.as_deref().unwrap_or("") != to.comment.as_deref().unwrap_or("");
                if !changed {
                    return Ok(Vec::new());
                }
                vec![format!("ALTER TABLE {} MODIFY COLUMN {}", table_sql(self, table)?, self.restated_column(table, from, to)?)]
            }
            Change::SetPrimaryKey { table, columns, previous } => {
                let t = table_sql(self, table)?;
                let had = previous.as_ref().is_some_and(|previous| !previous.columns.is_empty());
                let mut clauses = Vec::new();
                if had {
                    clauses.push("DROP PRIMARY KEY".to_owned());
                }
                if !columns.is_empty() {
                    clauses.push(format!("ADD PRIMARY KEY ({})", column_list(self, "primary key", columns)?));
                }
                if clauses.is_empty() {
                    return Ok(Vec::new());
                }
                // One statement: the table is never without a key in between.
                vec![format!("ALTER TABLE {t} {}", clauses.join(", "))]
            }
            Change::CreateIndex { table, index } => vec![format!(
                "CREATE {}INDEX {} ON {} ({})",
                if index.unique { "UNIQUE " } else { "" },
                ident(self, "index", &index.name)?,
                table_sql(self, table)?,
                index_columns(self, index)?
            )],
            Change::DropIndex { table, name } => {
                vec![format!("DROP INDEX {} ON {}", ident(self, "index", name)?, table_sql(self, table)?)]
            }
            Change::AddForeignKey { table, foreign_key } => {
                vec![format!("ALTER TABLE {} ADD {}", table_sql(self, table)?, named_fk(self, foreign_key, false)?)]
            }
            Change::DropForeignKey { table, name } => {
                vec![format!("ALTER TABLE {} DROP FOREIGN KEY {}", table_sql(self, table)?, ident(self, "foreign key", name)?)]
            }
            Change::Account(account) => return self.account_statements(account, Reveal::Redacted),
        })
    }

    fn account_statements(&self, change: &AccountChange, reveal: Reveal) -> Result<Vec<String>, DdlError> {
        crate::sql_account::mysql_statements(self, self.mariadb, change, reveal)
    }

    fn account_engine(&self) -> Option<AccountEngine> {
        Some(if self.mariadb { AccountEngine::Mariadb } else { AccountEngine::Mysql })
    }
}
