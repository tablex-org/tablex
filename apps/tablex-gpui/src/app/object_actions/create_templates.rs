//! The "New ..." templates of the navigator: a view, function, procedure, sequence, materialized
//! view, trigger or type of a schema, and a column, index, unique constraint, foreign key or
//! trigger of a table. Each is the text of a `CREATE` / `ALTER` statement for the connection's
//! dialect, put in a new SQL tab (never run): every name quoted for the dialect, placeholder names
//! (`new_view`, `column_name`) where the user fills in, and a comment line saying what to fill in.
//! The first placeholder is selected, so typing replaces it.
//!
//! The per-dialect table below is the only place that knows what a dialect can create and how it
//! spells it: the menus ask [`offered`], [`for_group`] and [`for_table_folder`], so a dialect that
//! cannot (PostgreSQL-only sequences, MySQL-family-only `ALTER TABLE ... ADD CONSTRAINT`) offers
//! nothing for it. A template is one statement the lexer of its dialect reads as one (PostgreSQL
//! bodies sit in `$$ ... $$`; the MySQL family's in a single statement, since `DELIMITER` is a
//! client command), so "Run current statement" runs all of it.
//!
//! Template syntax: `{ns}` the quoted namespace and a dot (nothing without a namespace), `{tbl}`
//! the table qualified like that, `{tbl1}` the table alone, `{q:name}` a quoted placeholder name.

use std::ops::Range;

use crate::connections::ConnectionKind;
use crate::navigator::{ObjectGroup, TableFolder};
use crate::sql_table::{AmbiguousName, SqlDialect};

/// What a "New ..." entry creates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum CreateKind {
    View,
    MaterializedView,
    Function,
    Procedure,
    Sequence,
    Trigger,
    Type,
    Column,
    Index,
    /// A unique constraint (SQLite: a unique index).
    Constraint,
    ForeignKey,
}

/// Where an entry is offered: on a schema or database row and its folders, or on a table row and
/// its folders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum Level {
    Namespace,
    Table,
}

impl CreateKind {
    pub(in crate::app) const ALL: [Self; 11] = [
        Self::View,
        Self::MaterializedView,
        Self::Function,
        Self::Procedure,
        Self::Sequence,
        Self::Trigger,
        Self::Type,
        Self::Column,
        Self::Index,
        Self::Constraint,
        Self::ForeignKey,
    ];

    fn at(self, level: Level) -> bool {
        match self {
            Self::View | Self::MaterializedView | Self::Function | Self::Procedure | Self::Sequence | Self::Type => level == Level::Namespace,
            // A trigger names its table in the template of a schema, and has it filled in on a table.
            Self::Trigger => true,
            Self::Column | Self::Index | Self::Constraint | Self::ForeignKey => level == Level::Table,
        }
    }

    pub(in crate::app) fn selector(self) -> &'static str {
        match self {
            Self::View => "object-create-view",
            Self::MaterializedView => "object-create-materialized-view",
            Self::Function => "object-create-function",
            Self::Procedure => "object-create-procedure",
            Self::Sequence => "object-create-sequence",
            Self::Trigger => "object-create-trigger",
            Self::Type => "object-create-type",
            Self::Column => "object-create-column",
            Self::Index => "object-create-index",
            Self::Constraint => "object-create-constraint",
            Self::ForeignKey => "object-create-foreign-key",
        }
    }

    /// The group of a schema that lists what the entry creates (a materialized view: the Views
    /// group of a dialect whose schema has no group of its own for them).
    fn group(self, kind: ConnectionKind) -> Option<ObjectGroup> {
        let has = |group| ObjectGroup::for_kind(Some(kind)).contains(&group);
        match self {
            Self::View => Some(ObjectGroup::Views),
            Self::MaterializedView => Some(if has(ObjectGroup::MaterializedViews) { ObjectGroup::MaterializedViews } else { ObjectGroup::Views }),
            Self::Function => Some(ObjectGroup::Functions),
            Self::Procedure => Some(ObjectGroup::Procedures),
            Self::Sequence => Some(ObjectGroup::Sequences),
            Self::Trigger => Some(ObjectGroup::Triggers),
            Self::Type => Some(ObjectGroup::Types),
            Self::Column | Self::Index | Self::Constraint | Self::ForeignKey => None,
        }
    }

    /// The folder under a table that lists what the entry creates.
    fn folder(self) -> Option<TableFolder> {
        match self {
            Self::Column => Some(TableFolder::Columns),
            Self::Index => Some(TableFolder::Indexes),
            Self::Constraint => Some(TableFolder::Constraints),
            Self::ForeignKey => Some(TableFolder::ForeignKeys),
            Self::Trigger => Some(TableFolder::Triggers),
            _ => None,
        }
    }
}

// ---- the per-dialect table ------------------------------------------------------------------------

type Table = &'static [(CreateKind, &'static str)];

const POSTGRES: Table = &[
    (CreateKind::View, "-- Name the view and write its query, then run the statement.\nCREATE VIEW {ns}{q:new_view} AS\nSELECT column_name\nFROM table_name;"),
    (
        CreateKind::MaterializedView,
        "-- Name the materialized view and write its query; WITH NO DATA leaves it empty until REFRESH MATERIALIZED VIEW.\nCREATE MATERIALIZED VIEW {ns}{q:new_materialized_view} AS\nSELECT column_name\nFROM table_name\nWITH DATA;",
    ),
    (
        CreateKind::Function,
        "-- Name the function, list its arguments and return type, and write its body.\nCREATE FUNCTION {ns}{q:new_function}(arg1 integer)\nRETURNS integer\nLANGUAGE plpgsql\nAS $$\nBEGIN\n  RETURN arg1;\nEND;\n$$;",
    ),
    (
        CreateKind::Procedure,
        "-- Name the procedure, list its arguments and write its body; run it with CALL.\nCREATE PROCEDURE {ns}{q:new_procedure}(arg1 integer)\nLANGUAGE plpgsql\nAS $$\nBEGIN\n  NULL;\nEND;\n$$;",
    ),
    (CreateKind::Sequence, "-- Name the sequence and set where it starts and how it steps.\nCREATE SEQUENCE {ns}{q:new_sequence}\n  START WITH 1\n  INCREMENT BY 1;"),
    (
        CreateKind::Trigger,
        "-- Name the trigger, choose the table and the event, and name the trigger function to call (it must return trigger).\nCREATE TRIGGER {q:new_trigger}\n  BEFORE INSERT ON {tbl}\n  FOR EACH ROW\n  EXECUTE FUNCTION {ns}{q:trigger_function}();",
    ),
    (CreateKind::Type, "-- Name the type and list its values (an enum); write AS (field type, ...) instead for a composite type.\nCREATE TYPE {ns}{q:new_type} AS ENUM ('value_1', 'value_2');"),
    (CreateKind::Column, "-- Name the column and set its type, then run the statement.\nALTER TABLE {tbl}\n  ADD COLUMN {q:new_column} text;"),
    (CreateKind::Index, "-- Name the index and list the columns it covers.\nCREATE INDEX {q:new_index}\n  ON {tbl} ({q:column_name});"),
    (
        CreateKind::Constraint,
        "-- Name the constraint and list the columns that must be unique (or write a CHECK instead).\nALTER TABLE {tbl}\n  ADD CONSTRAINT {q:new_constraint} UNIQUE ({q:column_name});",
    ),
    (
        CreateKind::ForeignKey,
        "-- Name the foreign key, the column, and the table and column it references.\nALTER TABLE {tbl}\n  ADD CONSTRAINT {q:new_foreign_key} FOREIGN KEY ({q:column_name})\n  REFERENCES {ns}{q:referenced_table} ({q:referenced_column});",
    ),
];

/// MySQL and MariaDB. A routine's or trigger's body is one statement: a `BEGIN ... END` block holds
/// `;` and needs the client's `DELIMITER`.
const MYSQL: Table = &[
    (CreateKind::View, "-- Name the view and write its query, then run the statement.\nCREATE VIEW {ns}{q:new_view} AS\nSELECT column_name\nFROM table_name;"),
    (
        CreateKind::Function,
        "-- Name the function, list its arguments and return type, and write its body (one statement).\nCREATE FUNCTION {ns}{q:new_function}(arg1 INT)\nRETURNS INT\nDETERMINISTIC\nRETURN arg1;",
    ),
    (
        CreateKind::Procedure,
        "-- Name the procedure, list its arguments and write its body (one statement); run it with CALL.\nCREATE PROCEDURE {ns}{q:new_procedure}(IN arg1 INT)\nSELECT arg1;",
    ),
    (
        CreateKind::Trigger,
        "-- Name the trigger, choose the table and the event, and write what it does (one statement).\nCREATE TRIGGER {ns}{q:new_trigger}\n  BEFORE INSERT ON {tbl}\n  FOR EACH ROW\n  SET NEW.column_name = NEW.column_name;",
    ),
    (CreateKind::Column, "-- Name the column and set its type, then run the statement.\nALTER TABLE {tbl}\n  ADD COLUMN {q:new_column} VARCHAR(255) NULL;"),
    (CreateKind::Index, "-- Name the index and list the columns it covers.\nCREATE INDEX {q:new_index}\n  ON {tbl} ({q:column_name});"),
    (
        CreateKind::Constraint,
        "-- Name the constraint and list the columns that must be unique (or write a CHECK instead).\nALTER TABLE {tbl}\n  ADD CONSTRAINT {q:new_constraint} UNIQUE ({q:column_name});",
    ),
    (
        CreateKind::ForeignKey,
        "-- Name the foreign key, the column, and the table and column it references.\nALTER TABLE {tbl}\n  ADD CONSTRAINT {q:new_foreign_key} FOREIGN KEY ({q:column_name})\n  REFERENCES {ns}{q:referenced_table} ({q:referenced_column});",
    ),
];

/// What MariaDB has beside the MySQL family's.
const MARIADB_ONLY: Table =
    &[(CreateKind::Sequence, "-- Name the sequence and set where it starts and how it steps.\nCREATE SEQUENCE {ns}{q:new_sequence}\n  START WITH 1\n  INCREMENT BY 1;")];

/// SQLite adds neither a constraint nor a foreign key to a table that exists: a unique index is its
/// unique constraint, and a foreign key means a new table. It has no trigger either: its body is a
/// `BEGIN ... END` block of `;`-ended statements, which the statement splitter reads as several
/// statements, so the template could not be run.
const SQLITE: Table = &[
    (CreateKind::View, "-- Name the view and write its query, then run the statement.\nCREATE VIEW {ns}{q:new_view} AS\nSELECT column_name\nFROM table_name;"),
    (CreateKind::Column, "-- Name the column and set its type, then run the statement.\nALTER TABLE {tbl}\n  ADD COLUMN {q:new_column} TEXT;"),
    (CreateKind::Index, "-- Name the index and list the columns it covers.\nCREATE INDEX {ns}{q:new_index}\n  ON {tbl1} ({q:column_name});"),
    (
        CreateKind::Constraint,
        "-- SQLite adds a unique constraint as a unique index: name it and list the columns that must be unique.\nCREATE UNIQUE INDEX {ns}{q:new_unique_index}\n  ON {tbl1} ({q:column_name});",
    ),
];

/// ClickHouse has no procedures, sequences, triggers, types, unique constraints or foreign keys; its
/// indexes are data-skipping ones.
const CLICKHOUSE: Table = &[
    (CreateKind::View, "-- Name the view and write its query, then run the statement.\nCREATE VIEW {ns}{q:new_view} AS\nSELECT column_name\nFROM table_name;"),
    (
        CreateKind::MaterializedView,
        "-- Name the materialized view, choose where its rows go (the engine) and write the query that fills it.\nCREATE MATERIALIZED VIEW {ns}{q:new_materialized_view}\nENGINE = MergeTree\nORDER BY tuple()\nAS SELECT column_name\nFROM table_name;",
    ),
    (CreateKind::Column, "-- Name the column and set its type, then run the statement.\nALTER TABLE {tbl}\n  ADD COLUMN {q:new_column} String;"),
    (CreateKind::Index, "-- Name the data-skipping index and the expression it covers.\nALTER TABLE {tbl}\n  ADD INDEX {q:new_index} {q:column_name} TYPE minmax GRANULARITY 1;"),
];

/// The tables that hold the templates of a connection kind (none for a kind that is not SQL).
fn tables(kind: ConnectionKind) -> [Table; 2] {
    match kind {
        ConnectionKind::Postgres => [POSTGRES, &[]],
        ConnectionKind::Mysql => [MYSQL, &[]],
        ConnectionKind::Mariadb => [MYSQL, MARIADB_ONLY],
        ConnectionKind::Sqlite => [SQLITE, &[]],
        ConnectionKind::Clickhouse => [CLICKHOUSE, &[]],
        ConnectionKind::Mongodb | ConnectionKind::Redis => [&[], &[]],
    }
}

fn source(kind: ConnectionKind, create: CreateKind) -> Option<&'static str> {
    tables(kind).into_iter().flatten().find(|(candidate, _)| *candidate == create).map(|(_, text)| *text)
}

// ---- what is offered where ------------------------------------------------------------------------

/// The entries a connection kind offers on a row of `level`, in menu order.
pub(in crate::app) fn offered(kind: ConnectionKind, level: Level) -> Vec<CreateKind> {
    CreateKind::ALL.into_iter().filter(|create| create.at(level) && source(kind, *create).is_some()).collect()
}

/// The entries of the object group `group` of a schema (Views: a view, and a materialized view for a
/// dialect with no group of its own for them).
pub(in crate::app) fn for_group(kind: ConnectionKind, group: ObjectGroup) -> Vec<CreateKind> {
    offered(kind, Level::Namespace).into_iter().filter(|create| create.group(kind) == Some(group)).collect()
}

/// The entry of a folder under a table.
pub(in crate::app) fn for_table_folder(kind: ConnectionKind, folder: TableFolder) -> Option<CreateKind> {
    offered(kind, Level::Table).into_iter().find(|create| create.folder() == Some(folder))
}

// ---- the text ---------------------------------------------------------------------------------------

/// The text of a template and where its first placeholder is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) struct Template {
    pub(in crate::app) text: String,
    /// The first placeholder (the name inside its quotes), to select.
    pub(in crate::app) select: Range<usize>,
}

/// The template of `create` for `kind`. `namespace` is the schema (the database, for the dialects
/// whose namespace is the database) the statement goes in; `table` is the table of a table-level
/// entry (a placeholder when `None`). `None`: the dialect cannot create it. A name the dialect cannot
/// quote unambiguously is refused.
pub(in crate::app) fn template(
    kind: ConnectionKind,
    dialect: SqlDialect,
    create: CreateKind,
    namespace: Option<&str>,
    table: Option<&str>,
) -> Option<Result<Template, AmbiguousName>> {
    let source = source(kind, create)?;
    Some(render(source, dialect, namespace.filter(|namespace| !namespace.is_empty()), table.unwrap_or("table_name")))
}

fn render(source: &str, dialect: SqlDialect, namespace: Option<&str>, table: &str) -> Result<Template, AmbiguousName> {
    let ns = match namespace {
        Some(namespace) => format!("{}.", dialect.checked_quote(namespace)?),
        None => String::new(),
    };
    let table = dialect.checked_quote(table)?;
    let mut text = String::with_capacity(source.len() + 64);
    let mut select = None;
    let mut rest = source;
    while let Some(open) = rest.find('{') {
        text.push_str(&rest[..open]);
        let close = rest[open..].find('}').map_or(rest.len(), |close| open + close);
        let token = &rest[open + 1..close];
        match token {
            "ns" => text.push_str(&ns),
            "tbl" => {
                text.push_str(&ns);
                text.push_str(&table);
            }
            "tbl1" => text.push_str(&table),
            _ => {
                let name = token.strip_prefix("q:").unwrap_or(token);
                let quoted = dialect.quote(name);
                // The name sits between the quote characters (one byte each).
                select.get_or_insert(text.len() + 1..text.len() + quoted.len() - 1);
                text.push_str(&quoted);
            }
        }
        rest = &rest[(close + 1).min(rest.len())..];
    }
    text.push_str(rest);
    Ok(Template { text, select: select.unwrap_or(0..0) })
}

/// The script "New Collection" opens on a MongoDB database: `createCollection` with a placeholder name.
pub(in crate::app) fn mongo_new_collection() -> Template {
    let text = "// Name the collection, then run the script.\ndb.createCollection(\"new_collection\")".to_owned();
    let start = text.find("new_collection").unwrap_or(0);
    Template { select: start..start + "new_collection".len(), text }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_driver_kit::sql::{classify, Keyword, SqlLexOptions};

    const SQL_KINDS: [ConnectionKind; 5] =
        [ConnectionKind::Postgres, ConnectionKind::Mysql, ConnectionKind::Mariadb, ConnectionKind::Sqlite, ConnectionKind::Clickhouse];

    fn lex(kind: ConnectionKind) -> SqlLexOptions {
        crate::app::data_transfer::order::lex_options(kind)
    }

    fn make(kind: ConnectionKind, create: CreateKind, namespace: Option<&str>, table: Option<&str>) -> Template {
        template(kind, SqlDialect::for_kind(kind), create, namespace, table).expect("offered").expect("quotable")
    }

    /// Every (kind, entry) a dialect has, with the namespace and table a row of its tree would give.
    fn every_template() -> Vec<(ConnectionKind, CreateKind, Template)> {
        let mut all = Vec::new();
        for kind in SQL_KINDS {
            let namespace = if kind == ConnectionKind::Sqlite { "main" } else { "ns" };
            for create in CreateKind::ALL {
                if source(kind, create).is_some() {
                    for table in [None, Some("orders")] {
                        all.push((kind, create, make(kind, create, Some(namespace), table)));
                    }
                }
            }
        }
        all
    }

    #[test]
    fn every_template_of_every_dialect_is_one_create_or_alter_statement_the_lexer_reads_whole() {
        let all = every_template();
        assert!(all.len() >= 60, "{}", all.len());
        for (kind, create, template) in all {
            let text = &template.text;
            let what = format!("{kind:?} {create:?}:\n{text}");
            assert!(!text.trim().is_empty(), "{what}");
            // One comment line says what to fill in, then the statement.
            let (comment, statement) = text.split_once('\n').expect(&what);
            assert!(comment.starts_with("-- ") && comment.len() > 10, "{what}");
            assert!(statement.starts_with("CREATE ") || statement.starts_with("ALTER "), "{what}");
            assert!(statement.ends_with(';') && !text.contains("{") && !text.contains("}"), "{what}");
            let classified = classify(text, &lex(kind));
            let [only] = classified.statements.as_slice() else { panic!("{} statements:\n{what}", classified.statements.len()) };
            assert!(matches!(only.keyword, Some(Keyword::Create | Keyword::Alter)), "{what}");
            assert!(!only.read_only && !classified.controls_transaction, "{what}");
            // Running an ALTER asks for confirmation, a CREATE does not.
            assert_eq!(classified.danger.is_some(), only.keyword == Some(Keyword::Alter), "{what}");
            // The statement splitter of "Run current statement" sees it whole too.
            let ranges = crate::sql_script::statement_ranges(text, &lex(kind));
            assert_eq!(ranges.len(), 1, "{what}");
            assert_eq!(&text[ranges[0].clone()], text.trim_end_matches(';'), "{what}");
        }
    }

    #[test]
    fn the_first_placeholder_is_selected_and_every_name_is_quoted_for_its_dialect() {
        for (kind, create, template) in every_template() {
            let quote = SqlDialect::for_kind(kind).identifier_quote;
            let selected = &template.text[template.select.clone()];
            assert!(selected.starts_with("new_") || selected == "column_name", "{kind:?} {create:?}: {selected}");
            let before = template.text[..template.select.start].chars().next_back();
            let after = template.text[template.select.end..].chars().next();
            assert_eq!((before, after), (Some(quote), Some(quote)), "{kind:?} {create:?}");
            // Neither quote of the other dialects is in the text, except in a string literal.
            let foreign = if quote == '`' { '"' } else { '`' };
            assert!(!template.text.contains(foreign), "{kind:?} {create:?}\n{}", template.text);
        }
    }

    #[test]
    fn what_each_dialect_offers_at_each_level() {
        use CreateKind::*;
        let ns = |kind| offered(kind, Level::Namespace);
        let table = |kind| offered(kind, Level::Table);
        assert_eq!(ns(ConnectionKind::Postgres), [View, MaterializedView, Function, Procedure, Sequence, Trigger, Type]);
        assert_eq!(table(ConnectionKind::Postgres), [Trigger, Column, Index, Constraint, ForeignKey]);
        assert_eq!(ns(ConnectionKind::Mysql), [View, Function, Procedure, Trigger]);
        assert_eq!(table(ConnectionKind::Mysql), [Trigger, Column, Index, Constraint, ForeignKey]);
        assert_eq!(ns(ConnectionKind::Mariadb), [View, Function, Procedure, Sequence, Trigger]);
        assert_eq!(table(ConnectionKind::Mariadb), table(ConnectionKind::Mysql));
        assert_eq!(ns(ConnectionKind::Sqlite), [View]);
        assert_eq!(table(ConnectionKind::Sqlite), [Column, Index, Constraint]);
        assert_eq!(ns(ConnectionKind::Clickhouse), [View, MaterializedView]);
        assert_eq!(table(ConnectionKind::Clickhouse), [Column, Index]);
        for kind in [ConnectionKind::Mongodb, ConnectionKind::Redis] {
            assert!(ns(kind).is_empty() && table(kind).is_empty(), "{kind:?}");
        }
    }

    #[test]
    fn a_folder_offers_what_it_lists() {
        use CreateKind::*;
        assert_eq!(for_group(ConnectionKind::Postgres, ObjectGroup::Views), [View]);
        assert_eq!(for_group(ConnectionKind::Postgres, ObjectGroup::MaterializedViews), [MaterializedView]);
        // ClickHouse lists its materialized views among the views.
        assert_eq!(for_group(ConnectionKind::Clickhouse, ObjectGroup::Views), [View, MaterializedView]);
        assert_eq!(for_group(ConnectionKind::Mysql, ObjectGroup::Sequences), []);
        assert_eq!(for_group(ConnectionKind::Mariadb, ObjectGroup::Sequences), [Sequence]);
        assert_eq!(for_group(ConnectionKind::Postgres, ObjectGroup::Tables), []);
        assert_eq!(for_table_folder(ConnectionKind::Postgres, TableFolder::ForeignKeys), Some(ForeignKey));
        assert_eq!(for_table_folder(ConnectionKind::Sqlite, TableFolder::ForeignKeys), None);
        assert_eq!(for_table_folder(ConnectionKind::Clickhouse, TableFolder::Constraints), None);
        assert_eq!(for_table_folder(ConnectionKind::Mysql, TableFolder::Triggers), Some(Trigger));
    }

    #[test]
    fn views_and_indexes_per_dialect() {
        let view = |kind, ns| make(kind, CreateKind::View, Some(ns), None).text;
        assert_eq!(view(ConnectionKind::Postgres, "public"), "-- Name the view and write its query, then run the statement.\nCREATE VIEW \"public\".\"new_view\" AS\nSELECT column_name\nFROM table_name;");
        assert!(view(ConnectionKind::Mysql, "shop").contains("CREATE VIEW `shop`.`new_view` AS"));
        assert!(view(ConnectionKind::Sqlite, "main").contains("CREATE VIEW \"main\".\"new_view\" AS"));
        assert!(view(ConnectionKind::Clickhouse, "db").contains("CREATE VIEW `db`.`new_view` AS"));
        let index = |kind, ns| make(kind, CreateKind::Index, Some(ns), Some("orders")).text;
        assert!(index(ConnectionKind::Postgres, "public").contains("CREATE INDEX \"new_index\"\n  ON \"public\".\"orders\" (\"column_name\");"));
        assert!(index(ConnectionKind::Mysql, "shop").contains("CREATE INDEX `new_index`\n  ON `shop`.`orders` (`column_name`);"));
        assert!(index(ConnectionKind::Sqlite, "main").contains("CREATE INDEX \"main\".\"new_index\"\n  ON \"orders\" (\"column_name\");"));
        assert!(index(ConnectionKind::Clickhouse, "db").contains("ALTER TABLE `db`.`orders`\n  ADD INDEX `new_index` `column_name` TYPE minmax GRANULARITY 1;"));
    }

    #[test]
    fn a_hostile_name_is_quoted_with_its_quote_doubled_and_a_name_the_dialect_cannot_quote_is_refused() {
        let text = make(ConnectionKind::Postgres, CreateKind::Column, Some("we\"ird"), Some("a\"b")).text;
        assert!(text.contains("ALTER TABLE \"we\"\"ird\".\"a\"\"b\""), "{text}");
        let text = make(ConnectionKind::Mysql, CreateKind::Column, Some("a`b"), Some("t;x")).text;
        assert!(text.contains("ALTER TABLE `a``b`.`t;x`"), "{text}");
        // The quotes keep the statement one statement.
        assert_eq!(classify(&text, &lex(ConnectionKind::Mysql)).statements.len(), 1);
        let refused = template(ConnectionKind::Clickhouse, SqlDialect::CLICKHOUSE, CreateKind::Column, Some("db"), Some("back\\slash")).unwrap();
        assert!(refused.is_err());
    }

    #[test]
    fn a_dialect_that_cannot_has_no_template() {
        assert!(template(ConnectionKind::Mysql, SqlDialect::MYSQL, CreateKind::Sequence, None, None).is_none());
        assert!(template(ConnectionKind::Clickhouse, SqlDialect::CLICKHOUSE, CreateKind::ForeignKey, None, None).is_none());
        assert!(template(ConnectionKind::Sqlite, SqlDialect::SQLITE, CreateKind::ForeignKey, None, None).is_none());
    }

    #[test]
    fn without_a_namespace_names_are_not_qualified() {
        let text = make(ConnectionKind::Sqlite, CreateKind::Index, None, Some("orders")).text;
        assert!(text.contains("CREATE INDEX \"new_index\"\n  ON \"orders\""), "{text}");
        let text = make(ConnectionKind::Postgres, CreateKind::Trigger, Some("public"), None).text;
        assert!(text.contains("BEFORE INSERT ON \"public\".\"table_name\""), "{text}");
        let text = make(ConnectionKind::Postgres, CreateKind::Trigger, Some("public"), Some("orders")).text;
        assert!(text.contains("BEFORE INSERT ON \"public\".\"orders\""), "{text}");
    }

    #[test]
    fn the_mongo_script_selects_its_placeholder_and_is_a_shell_script() {
        let template = mongo_new_collection();
        assert_eq!(&template.text[template.select.clone()], "new_collection");
        assert!(matches!(crate::mongo_script::classify(&template.text), crate::mongo_script::ScriptKind::Shell));
    }
}
