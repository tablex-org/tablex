//! "Generate SQL" of a table or view row: a SELECT, INSERT, UPDATE or DELETE for the object, opened
//! in a new SQL tab bound to its connection and namespace. The text lists the object's real
//! columns (one `describe_object`, off the UI thread), every name quoted for the connection's
//! dialect, and is only put in the editor: it never runs on its own.
//!
//! * INSERT leaves out generated columns and identity / serial / auto-increment columns (the server
//!   fills them); UPDATE sets every other column except the primary key (the WHERE) and those same
//!   server-filled columns.
//! * A parameter is `:name` (what the editor binds). A column whose name is not a valid parameter
//!   name (a quote, a space) gets a sanitized, unique one.
//! * A connection kind whose primary key does not identify a row (ClickHouse) gets only SELECT and
//!   INSERT.
//! * A table with no primary key gets a `WHERE <condition>` line, which is not SQL: a bare UPDATE
//!   or DELETE of every row cannot be run by accident.

use engine_driver_api::sql::TableColumn;

use super::super::*;
use crate::connections::ConnectionKind;
use crate::properties_model::PropertiesKey;
use crate::sql_ddl::dialect_for;
use crate::sql_table::{qualified_table, AmbiguousName, SqlDialect};

/// Rows a generated SELECT reads.
const SELECT_ROWS: u32 = 100;
/// More columns than this go one per line.
const INLINE_COLUMNS: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum GenerateKind {
    Select,
    Insert,
    Update,
    Delete,
}

impl GenerateKind {
    pub(in crate::app) const ALL: [Self; 4] = [Self::Select, Self::Insert, Self::Update, Self::Delete];

    pub(in crate::app) fn label(self) -> &'static str {
        match self {
            Self::Select => "SELECT",
            Self::Insert => "INSERT",
            Self::Update => "UPDATE",
            Self::Delete => "DELETE",
        }
    }

    pub(in crate::app) fn selector(self) -> &'static str {
        match self {
            Self::Select => "object-generate-select",
            Self::Insert => "object-generate-insert",
            Self::Update => "object-generate-update",
            Self::Delete => "object-generate-delete",
        }
    }

    pub(in crate::app) fn icon(self) -> Icon {
        let path = match self {
            Self::Select => "icons/eye.svg",
            Self::Insert => "icons/circle-plus.svg",
            Self::Update => "icons/pencil.svg",
            Self::Delete => "icons/delete.svg",
        };
        Icon::empty().path(path).size(px(12.0))
    }
}

/// How a dialect limits the rows of a SELECT; `None` for a kind that is not SQL (it gets no Generate
/// SQL). Every supported dialect takes `LIMIT n`; a dialect with `TOP` or `FETCH FIRST` adds its arm.
fn row_limit(kind: ConnectionKind) -> Option<String> {
    match kind {
        ConnectionKind::Postgres | ConnectionKind::Mysql | ConnectionKind::Mariadb | ConnectionKind::Sqlite | ConnectionKind::Clickhouse => {
            Some(format!("LIMIT {SELECT_ROWS}"))
        }
        ConnectionKind::Mongodb | ConnectionKind::Redis => None,
    }
}

/// Whether a row of the connection kind's tables can be told apart by its primary key, which is what
/// the WHERE of an UPDATE or DELETE rests on. ClickHouse's `primary_key` is a sorting-key prefix
/// (`system.columns.is_in_primary_key`): many rows share it, so `WHERE key = :key` would touch all of
/// them, and a plain `UPDATE` is not ClickHouse syntax before 25.x. Such a kind gets SELECT and INSERT.
fn has_row_identifying_key(kind: ConnectionKind) -> bool {
    !matches!(kind, ConnectionKind::Clickhouse)
}

/// The statement of `kind` for `table`, with `columns` as described.
pub(super) fn generate(
    kind: GenerateKind,
    dialect: SqlDialect,
    limit: &str,
    schema: Option<&str>,
    table: &str,
    columns: &[TableColumn],
) -> Result<String, AmbiguousName> {
    let target = qualified_table(dialect, schema, table)?;
    let names = parameter_names(columns);
    // (quoted column, parameter, the column)
    let all = columns
        .iter()
        .zip(&names)
        .map(|(column, name)| Ok((dialect.checked_quote(&column.name)?, format!(":{name}"), column)))
        .collect::<Result<Vec<_>, AmbiguousName>>()?;
    // Generated and identity / auto-increment columns are filled by the server.
    let fillable = |column: &TableColumn| column.is_generated != Some(true) && column.auto_increment != Some(true);
    Ok(match kind {
        GenerateKind::Select => {
            let listed: Vec<String> = all.iter().map(|(quoted, ..)| quoted.clone()).collect();
            let head = match listed.len() {
                0 => "SELECT *".to_owned(),
                n if n > INLINE_COLUMNS => format!("SELECT\n  {}", listed.join(",\n  ")),
                _ => format!("SELECT {}", listed.join(", ")),
            };
            format!("{head}\nFROM {target}\n{limit};")
        }
        GenerateKind::Insert => {
            let fillable: Vec<_> = all.iter().filter(|(.., column)| fillable(column)).collect();
            let (columns, values) = if fillable.is_empty() {
                ("(<columns>)".to_owned(), "(<values>)".to_owned())
            } else {
                let quoted: Vec<&str> = fillable.iter().map(|(quoted, ..)| quoted.as_str()).collect();
                let params: Vec<&str> = fillable.iter().map(|(_, param, _)| param.as_str()).collect();
                if fillable.len() > INLINE_COLUMNS {
                    (format!("(\n  {}\n)", quoted.join(",\n  ")), format!("(\n  {}\n)", params.join(",\n  ")))
                } else {
                    (format!("({})", quoted.join(", ")), format!("({})", params.join(", ")))
                }
            };
            format!("INSERT INTO {target} {columns}\nVALUES {values};")
        }
        GenerateKind::Update => {
            let assignments: Vec<String> =
                all.iter().filter(|(.., column)| fillable(column) && !column.primary_key).map(|(quoted, param, _)| format!("{quoted} = {param}")).collect();
            let set = match assignments.len() {
                0 => "SET <assignments>".to_owned(),
                n if n > INLINE_COLUMNS => format!("SET\n  {}", assignments.join(",\n  ")),
                _ => format!("SET {}", assignments.join(", ")),
            };
            format!("UPDATE {target}\n{set}\n{};", where_clause(&all))
        }
        GenerateKind::Delete => format!("DELETE FROM {target}\n{};", where_clause(&all)),
    })
}

/// `WHERE pk = :pk`, or the `WHERE <condition>` placeholder of a table with no primary key.
fn where_clause(columns: &[(String, String, &TableColumn)]) -> String {
    let keys: Vec<String> = columns.iter().filter(|(.., column)| column.primary_key).map(|(quoted, param, _)| format!("{quoted} = {param}")).collect();
    if keys.is_empty() {
        "WHERE <condition>".to_owned()
    } else {
        format!("WHERE {}", keys.join("\n  AND "))
    }
}

/// One parameter name per column: its own name when that is a valid one, else the name with every
/// other character turned into `_`; a repeated name gets `_2`, `_3`, ...
fn parameter_names(columns: &[TableColumn]) -> Vec<String> {
    let mut used = std::collections::HashSet::new();
    columns
        .iter()
        .map(|column| {
            let mut base: String = column.name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' }).collect();
            if !base.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
                base.insert(0, '_');
            }
            let mut name = base.clone();
            let mut n = 2;
            while !used.insert(name.clone()) {
                name = format!("{base}_{n}");
                n += 1;
            }
            name
        })
        .collect()
}

impl AppRoot {
    /// The statements the row `id` offers: all four for a table (SELECT and INSERT where the primary
    /// key does not identify a row), a SELECT for a view, none for a row that is not a SQL object (a
    /// MongoDB collection, a Redis key, a namespace).
    pub(in crate::app) fn generate_sql_kinds(&self, id: &NavigatorNodeId) -> &'static [GenerateKind] {
        let NavigatorNodeId::Object { connection, .. } = id else { return &[] };
        let Some(saved) = self.saved_connections.iter().find(|saved| saved.matches_id(connection)) else { return &[] };
        if dialect_for(saved.kind).is_none() || row_limit(saved.kind).is_none() {
            return &[];
        }
        match super::ObjectKind::of(self.navigator.object_kind(id)) {
            Some(super::ObjectKind::Table) if has_row_identifying_key(saved.kind) => &GenerateKind::ALL,
            Some(super::ObjectKind::Table) => &GenerateKind::ALL[..2],
            Some(super::ObjectKind::View | super::ObjectKind::MaterializedView) => &GenerateKind::ALL[..1],
            None => &[],
        }
    }

    /// Describes the object of the row `id`, then opens a new SQL tab on it with the statement. The
    /// describe runs off the UI thread; a failure (or a name the dialect cannot quote) is reported
    /// and no tab opens.
    pub(in crate::app) fn open_generated_sql(&mut self, kind: GenerateKind, id: &NavigatorNodeId, cx: &mut Context<Self>) {
        let NavigatorNodeId::Object { connection, database, schema, object } = id else { return };
        if !self.generate_sql_kinds(id).contains(&kind) {
            return;
        }
        let key = PropertiesKey {
            connection: connection.clone(),
            database: database.clone(),
            schema: schema.clone(),
            object: object.clone(),
            session: self.driver_runtime.connection_ids.get(connection).copied().unwrap_or(0),
        };
        let id = id.clone();
        self.describe_table(key, cx, move |this, described, cx| {
            let sql = described.and_then(|properties| this.generated_sql(kind, &id, &properties.columns));
            match sql {
                Ok(sql) => {
                    if this.open_new_query_for_node(&id, cx) {
                        if let Some(tab) = this.workspace.active_tab_mut() {
                            tab.sql = sql;
                        }
                        cx.notify();
                    }
                }
                Err(error) => {
                    let message = format!("Could not generate {}: {error}", kind.label());
                    if let NavigatorNodeId::Object { connection, .. } = &id {
                        this.report_error(Area::Query, Some(connection), message.clone());
                    }
                    this.sidebar.notice = Some(message);
                    cx.notify();
                }
            }
        });
    }

    fn generated_sql(&self, kind: GenerateKind, id: &NavigatorNodeId, columns: &[TableColumn]) -> Result<String, String> {
        let NavigatorNodeId::Object { connection, database, schema, object } = id else { return Err("not a table".into()) };
        let saved = self.saved_connections.iter().find(|saved| saved.matches_id(connection)).ok_or("the connection is gone")?;
        let (dialect, limit) = (self.sql_dialect_for_connection(connection).ok_or("the connection has no SQL dialect")?, row_limit(saved.kind).ok_or("not a SQL connection")?);
        // MySQL, MariaDB and ClickHouse qualify with the database.
        let namespace = Some(schema.as_str())
            .filter(|schema| !schema.is_empty())
            .or_else(|| dialect_for(saved.kind).filter(|ddl| ddl.schema_is_database()).and(Some(database.as_str())).filter(|database| !database.is_empty()));
        generate(kind, dialect, &limit, namespace, object, columns).map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn column(name: &str, primary_key: bool) -> TableColumn {
        TableColumn { name: name.into(), data_type: "text".into(), primary_key, ..TableColumn::default() }
    }

    fn make(kind: GenerateKind, dialect: SqlDialect, schema: Option<&str>, columns: &[TableColumn]) -> String {
        generate(kind, dialect, "LIMIT 100", schema, "t", columns).unwrap()
    }

    #[test]
    fn mysql_quotes_with_backticks_and_postgres_with_double_quotes_and_each_doubles_its_own_quote() {
        let columns = [column("id", true), column("we`ird\"x", false)];
        assert_eq!(
            make(GenerateKind::Select, SqlDialect::MYSQL, Some("shop"), &columns),
            "SELECT `id`, `we``ird\"x`\nFROM `shop`.`t`\nLIMIT 100;"
        );
        assert_eq!(
            make(GenerateKind::Select, SqlDialect::POSTGRES, Some("public"), &columns),
            "SELECT \"id\", \"we`ird\"\"x\"\nFROM \"public\".\"t\"\nLIMIT 100;"
        );
        assert_eq!(
            make(GenerateKind::Update, SqlDialect::MYSQL, Some("shop"), &columns),
            "UPDATE `shop`.`t`\nSET `we``ird\"x` = :we_ird_x\nWHERE `id` = :id;"
        );
    }

    #[test]
    fn insert_leaves_out_generated_and_auto_increment_columns_and_update_leaves_out_generated_and_key_columns() {
        let mut serial = column("id", true);
        serial.auto_increment = Some(true);
        let mut total = column("total", false);
        total.is_generated = Some(true);
        let columns = [serial, column("name", false), total];
        assert_eq!(make(GenerateKind::Insert, SqlDialect::POSTGRES, None, &columns), "INSERT INTO \"t\" (\"name\")\nVALUES (:name);");
        assert_eq!(make(GenerateKind::Update, SqlDialect::POSTGRES, None, &columns), "UPDATE \"t\"\nSET \"name\" = :name\nWHERE \"id\" = :id;");
    }

    #[test]
    fn a_statement_with_nothing_to_list_keeps_a_placeholder_that_is_not_runnable_sql() {
        let mut serial = column("id", true);
        serial.auto_increment = Some(true);
        let only_key = [serial];
        assert_eq!(make(GenerateKind::Insert, SqlDialect::MYSQL, None, &only_key), "INSERT INTO `t` (<columns>)\nVALUES (<values>);");
        assert_eq!(make(GenerateKind::Update, SqlDialect::MYSQL, None, &only_key), "UPDATE `t`\nSET <assignments>\nWHERE `id` = :id;");
        assert_eq!(make(GenerateKind::Select, SqlDialect::MYSQL, None, &[]), "SELECT *\nFROM `t`\nLIMIT 100;");
        assert_eq!(make(GenerateKind::Delete, SqlDialect::MYSQL, None, &[column("a", false)]), "DELETE FROM `t`\nWHERE <condition>;");
    }

    #[test]
    fn more_than_three_columns_go_one_per_line() {
        let columns: Vec<_> = ["a", "b", "c", "d"].into_iter().map(|name| column(name, false)).collect();
        assert_eq!(make(GenerateKind::Select, SqlDialect::POSTGRES, None, &columns), "SELECT\n  \"a\",\n  \"b\",\n  \"c\",\n  \"d\"\nFROM \"t\"\nLIMIT 100;");
        assert_eq!(make(GenerateKind::Select, SqlDialect::POSTGRES, None, &columns[..3]), "SELECT \"a\", \"b\", \"c\"\nFROM \"t\"\nLIMIT 100;");
    }

    #[test]
    fn every_parameter_is_found_by_the_editor_and_no_two_columns_share_one() {
        let columns = [column("id", true), column("a b", false), column("a_b", false), column("a-b", false), column("9", false), column("", false)];
        let sql = make(GenerateKind::Update, SqlDialect::POSTGRES, None, &columns);
        let lex = engine_driver_kit::sql::SqlLexOptions::postgres();
        let found: Vec<String> = crate::sql_params::find(&sql, &lex).into_iter().map(|parameter| parameter.name).collect();
        assert_eq!(found, ["a_b", "a_b_2", "a_b_3", "_9", "_", "id"], "{sql}");
    }

    #[test]
    fn a_name_the_dialect_cannot_quote_unambiguously_is_refused() {
        let columns = [column("back\\slash", false)];
        assert!(generate(GenerateKind::Select, SqlDialect::CLICKHOUSE, "LIMIT 100", None, "t", &columns).is_err());
    }

    #[test]
    fn only_clickhouse_lacks_a_row_identifying_key() {
        assert!(!has_row_identifying_key(ConnectionKind::Clickhouse));
        for kind in [ConnectionKind::Postgres, ConnectionKind::Mysql, ConnectionKind::Mariadb, ConnectionKind::Sqlite] {
            assert!(has_row_identifying_key(kind));
        }
    }

    #[test]
    fn update_sets_neither_generated_identity_nor_key_columns() {
        let mut identity = column("seq", false);
        identity.auto_increment = Some(true);
        let mut total = column("total", false);
        total.is_generated = Some(true);
        let columns = [column("id", true), identity, total, column("name", false)];
        assert_eq!(make(GenerateKind::Update, SqlDialect::POSTGRES, None, &columns), "UPDATE \"t\"\nSET \"name\" = :name\nWHERE \"id\" = :id;");
    }

    #[test]
    fn only_sql_kinds_have_a_row_limit() {
        assert!(row_limit(ConnectionKind::Mysql).is_some() && row_limit(ConnectionKind::Clickhouse).is_some());
        assert!(row_limit(ConnectionKind::Mongodb).is_none() && row_limit(ConnectionKind::Redis).is_none());
    }
}
