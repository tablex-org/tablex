//! The statements of the navigator's object actions (drop, rename, truncate, create schema /
//! database) that every dialect words the same way, built from the dialect's own quoting and
//! [`ObjectCapabilities`]. What a dialect says differently (renaming a view, dropping a
//! materialized view) stays in its own `statements`.

use super::{ident, table_sql, DdlDialect, DdlError, NamespaceKind, TableId};

/// The quoted identifier of a name the user typed for a new object: [`ident`]'s checks, and no
/// leading or trailing space, no control character (a newline, a tab, a bell): servers drop or
/// reject them and the name could not be told apart from another on screen. Where a backslash in
/// a quoted name is read differently by servers the dialect stands for (ClickHouse escapes with
/// it, MySQL under `ANSI_QUOTES` does not) the name is refused: the app does not make one that
/// its own table tabs could not query.
pub fn checked_name(d: &dyn DdlDialect, what: &str, name: &str) -> Result<String, DdlError> {
    let quoted = ident(d, what, name)?;
    if name != name.trim() {
        return Err(DdlError::Invalid(format!("The {what} name \"{}\" starts or ends with a space.", name.escape_debug())));
    }
    if name.chars().any(char::is_control) {
        return Err(DdlError::Invalid(format!("The {what} name \"{}\" contains a control character.", name.escape_debug())));
    }
    if name.contains('\\') && d.quoting().backslash_in_name_is_ambiguous() {
        return Err(DdlError::Invalid(format!(
            "The {what} name \"{}\" has a backslash, which cannot be quoted unambiguously for {}: pick a name without one.",
            name.escape_debug(),
            d.label()
        )));
    }
    Ok(quoted)
}

/// `DROP <what> <table> [CASCADE]`: CASCADE only where the dialect has it.
pub(super) fn drop_statement(d: &dyn DdlDialect, what: &str, table: &TableId, cascade: bool) -> Result<String, DdlError> {
    if cascade && !d.object_capabilities().drop_cascade {
        return Err(DdlError::Unsupported(format!("{} has no DROP ... CASCADE; drop what depends on it first.", d.label())));
    }
    Ok(format!("DROP {what} {}{}", table_sql(d, table)?, if cascade { " CASCADE" } else { "" }))
}

/// The keyword of dropping a view: `VIEW`, or `MATERIALIZED VIEW` where the dialect has them.
pub(super) fn view_keyword(d: &dyn DdlDialect, materialized: bool) -> Result<&'static str, DdlError> {
    match (materialized, d.object_capabilities().materialized_views) {
        (false, _) => Ok("VIEW"),
        (true, true) => Ok("MATERIALIZED VIEW"),
        (true, false) => Err(DdlError::Unsupported(format!("{} has no materialized views.", d.label()))),
    }
}

/// `TRUNCATE TABLE`, or `DELETE FROM` where the dialect has no TRUNCATE.
pub(super) fn truncate_statements(d: &dyn DdlDialect, table: &TableId, restart_identity: bool, cascade: bool) -> Result<Vec<String>, DdlError> {
    let caps = d.object_capabilities();
    if (restart_identity || cascade) && !caps.truncate_options {
        return Err(DdlError::Unsupported(format!("{} has no RESTART IDENTITY or CASCADE for TRUNCATE.", d.label())));
    }
    let table = table_sql(d, table)?;
    if !caps.truncate_statement {
        return Ok(vec![format!("DELETE FROM {table}")]);
    }
    let mut sql = format!("TRUNCATE TABLE {table}");
    if restart_identity {
        sql.push_str(" RESTART IDENTITY");
    }
    if cascade {
        sql.push_str(" CASCADE");
    }
    Ok(vec![sql])
}

/// `CREATE SCHEMA` / `CREATE DATABASE`, for the kind the dialect creates.
pub(super) fn create_namespace(d: &dyn DdlDialect, kind: NamespaceKind, name: &str) -> Result<Vec<String>, DdlError> {
    match d.object_capabilities().create_namespace {
        [] => Err(DdlError::Unsupported(format!(
            "{} has no schemas or databases to create: a SQLite database is a file, open or attach another one.",
            d.label()
        ))),
        supported if supported.contains(&kind) => {
            let keyword = if kind == NamespaceKind::Schema { "SCHEMA" } else { "DATABASE" };
            Ok(vec![format!("CREATE {keyword} {}", checked_name(d, kind.noun(), name)?)])
        }
        supported => {
            let kinds: Vec<&str> = supported.iter().map(|kind| kind.noun()).collect();
            Err(DdlError::Unsupported(format!("{} creates {}s, not {}s.", d.label(), kinds.join(" and "), kind.noun())))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::draft::Draft;
    use super::super::*;
    use crate::connections::ConnectionKind::{self, Mariadb, Mysql, Postgres, Sqlite};
    use engine_driver_kit::sql::SqlLexOptions;

    fn dialect(kind: ConnectionKind) -> &'static dyn DdlDialect {
        dialect_for(kind).expect("a SQL kind")
    }

    fn table(schema: &str, name: &str) -> TableId {
        TableId::new(Some(schema), name)
    }

    fn sql(kind: ConnectionKind, change: Change) -> Vec<String> {
        dialect(kind).statements(&change, &[]).unwrap_or_else(|error| panic!("{kind:?}: {error}"))
    }

    fn refused(kind: ConnectionKind, change: Change) -> String {
        dialect(kind).statements(&change, &[]).expect_err("is refused").to_string()
    }

    fn drop_table(table: TableId, cascade: bool) -> Change {
        Change::DropTable { table, cascade }
    }

    fn drop_view(table: TableId, materialized: bool, cascade: bool) -> Change {
        Change::DropView { table, materialized, cascade }
    }

    fn truncate(table: TableId, restart_identity: bool, cascade: bool) -> Change {
        Change::TruncateTable { table, restart_identity, cascade }
    }

    #[test]
    fn postgres_drops_renames_truncates_and_creates_a_schema() {
        let t = table("shop", "orders");
        assert_eq!(sql(Postgres, drop_table(t.clone(), false)), ["DROP TABLE \"shop\".\"orders\""]);
        assert_eq!(sql(Postgres, drop_table(t.clone(), true)), ["DROP TABLE \"shop\".\"orders\" CASCADE"]);
        assert_eq!(sql(Postgres, drop_view(t.clone(), false, false)), ["DROP VIEW \"shop\".\"orders\""]);
        assert_eq!(sql(Postgres, drop_view(t.clone(), true, true)), ["DROP MATERIALIZED VIEW \"shop\".\"orders\" CASCADE"]);
        assert_eq!(
            sql(Postgres, Change::RenameView { table: t.clone(), to: "sales".into(), materialized: false }),
            ["ALTER VIEW \"shop\".\"orders\" RENAME TO \"sales\""]
        );
        assert_eq!(
            sql(Postgres, Change::RenameView { table: t.clone(), to: "sales".into(), materialized: true }),
            ["ALTER MATERIALIZED VIEW \"shop\".\"orders\" RENAME TO \"sales\""]
        );
        assert_eq!(sql(Postgres, truncate(t.clone(), false, false)), ["TRUNCATE TABLE \"shop\".\"orders\""]);
        assert_eq!(sql(Postgres, truncate(t, true, true)), ["TRUNCATE TABLE \"shop\".\"orders\" RESTART IDENTITY CASCADE"]);
        assert_eq!(sql(Postgres, Change::CreateNamespace { kind: NamespaceKind::Schema, name: "reports".into() }), ["CREATE SCHEMA \"reports\""]);
        assert_eq!(sql(Postgres, Change::CreateNamespace { kind: NamespaceKind::Database, name: "reports".into() }), ["CREATE DATABASE \"reports\""]);
    }

    #[test]
    fn mysql_and_mariadb_refuse_what_they_do_not_have() {
        let t = table("shop", "orders");
        for kind in [Mysql, Mariadb] {
            assert_eq!(sql(kind, drop_table(t.clone(), false)), ["DROP TABLE `shop`.`orders`"]);
            assert_eq!(sql(kind, drop_view(t.clone(), false, false)), ["DROP VIEW `shop`.`orders`"]);
            assert_eq!(
                sql(kind, Change::RenameView { table: t.clone(), to: "sales".into(), materialized: false }),
                ["RENAME TABLE `shop`.`orders` TO `shop`.`sales`"]
            );
            assert_eq!(sql(kind, truncate(t.clone(), false, false)), ["TRUNCATE TABLE `shop`.`orders`"]);
            assert_eq!(sql(kind, Change::CreateNamespace { kind: NamespaceKind::Database, name: "reports".into() }), ["CREATE DATABASE `reports`"]);
            assert!(refused(kind, drop_table(t.clone(), true)).contains("no DROP ... CASCADE"));
            assert!(refused(kind, drop_view(t.clone(), true, false)).contains("no materialized views"));
            assert!(refused(kind, truncate(t.clone(), true, false)).contains("no RESTART IDENTITY or CASCADE"));
            assert!(refused(kind, Change::CreateNamespace { kind: NamespaceKind::Schema, name: "x".into() }).contains("creates databases, not schemas"));
        }
    }

    #[test]
    fn sqlite_deletes_instead_of_truncating_and_cannot_rename_a_view_or_create_a_namespace() {
        let t = table("main", "items");
        assert_eq!(sql(Sqlite, truncate(t.clone(), false, false)), ["DELETE FROM \"main\".\"items\""]);
        assert_eq!(sql(Sqlite, drop_view(t.clone(), false, false)), ["DROP VIEW \"main\".\"items\""]);
        assert!(refused(Sqlite, truncate(t.clone(), true, false)).contains("no RESTART IDENTITY"));
        assert!(refused(Sqlite, drop_table(t.clone(), true)).contains("no DROP ... CASCADE"));
        assert!(refused(Sqlite, Change::RenameView { table: t, to: "x".into(), materialized: false }).contains("cannot rename the view"));
        assert!(refused(Sqlite, Change::CreateNamespace { kind: NamespaceKind::Schema, name: "x".into() }).contains("no schemas or databases to create"));
    }

    #[test]
    fn clickhouse_drops_views_with_drop_view_and_creates_databases() {
        let t = table("shop", "events");
        assert_eq!(sql(ConnectionKind::Clickhouse, drop_view(t.clone(), false, false)), ["DROP VIEW `shop`.`events`"]);
        assert_eq!(sql(ConnectionKind::Clickhouse, truncate(t, false, false)), ["TRUNCATE TABLE `shop`.`events`"]);
        assert_eq!(
            sql(ConnectionKind::Clickhouse, Change::CreateNamespace { kind: NamespaceKind::Database, name: "reports".into() }),
            ["CREATE DATABASE `reports`"]
        );
    }

    /// Names that try to end the identifier or the statement.
    const HOSTILE: [&str; 9] = [
        "x\\",
        "x\\`, db.v --",
        "\\\"; DROP TABLE users; --",
        "x\"; DROP TABLE users; --",
        "x`; DROP TABLE users; --",
        "x'; DROP TABLE users; --",
        "x]; DROP TABLE users; --",
        "a;b",
        "\"\"\"`'`",
    ];

    fn lexer(kind: ConnectionKind) -> SqlLexOptions {
        match kind {
            Postgres => SqlLexOptions::postgres(),
            Sqlite => SqlLexOptions::sqlite(),
            ConnectionKind::Clickhouse => SqlLexOptions::clickhouse(),
            _ => SqlLexOptions::mysql(),
        }
    }

    #[test]
    fn hostile_names_stay_one_quoted_identifier_in_one_statement() {
        for kind in [Postgres, Mysql, Mariadb, Sqlite, ConnectionKind::Clickhouse] {
            let d = dialect(kind);
            let caps = d.object_capabilities();
            for name in HOSTILE {
                let quoted = d.quote(name);
                let mut changes = vec![
                    drop_table(table(name, name), false),
                    drop_view(table(name, name), false, false),
                    truncate(table(name, name), false, false),
                ];
                if caps.rename_view {
                    changes.push(Change::RenameView { table: table(name, name), to: name.into(), materialized: false });
                }
                for &namespace in caps.create_namespace {
                    changes.push(Change::CreateNamespace { kind: namespace, name: name.into() });
                }
                for change in changes {
                    let statements = match d.statements(&change, &[]) {
                        Ok(statements) => statements,
                        // A new name with a backslash is refused on ClickHouse (see `checked_name`).
                        Err(error) if kind == ConnectionKind::Clickhouse && name.contains('\\') && error.to_string().contains("has a backslash") => continue,
                        Err(error) => panic!("{kind:?} {name:?}: {error}"),
                    };
                    assert_eq!(statements.len(), 1, "{statements:?}");
                    let statement = &statements[0];
                    assert!(statement.contains(&quoted), "{kind:?}: {quoted} in {statement}");
                    // The server lexes it as one statement: no `;` or quote escapes the identifier.
                    let ranges = crate::sql_script::statement_ranges(statement, &lexer(kind));
                    assert_eq!(ranges.len(), 1, "{kind:?}: {statement} splits into {ranges:?}");
                    // Stripping the quoted names leaves the plain statement and nothing else.
                    let bare = statement.replace(&quoted, "?");
                    assert!(!bare.contains(';') && !bare.contains("DROP TABLE users"), "{kind:?}: {bare}");
                }
            }
        }
    }

    #[test]
    fn clickhouse_doubles_a_backslash_in_a_name_because_it_escapes_inside_backticks() {
        let ch = dialect(ConnectionKind::Clickhouse);
        assert_eq!(ch.quote("x\\`, db.v --"), "`x\\\\``, db.v --`");
        assert_eq!(ch.quote("plain"), "`plain`");
        // A table reference quotes both parts the same way, and the other servers keep a backslash as it is.
        let t = TableId::new(Some("d\\b"), "t\\");
        assert_eq!(sql(ConnectionKind::Clickhouse, drop_table(t.clone(), false)), ["DROP TABLE `d\\\\b`.`t\\\\`"]);
        assert_eq!(sql(Mysql, drop_table(t, false)), ["DROP TABLE `d\\b`.`t\\`"]);
    }

    #[test]
    fn postgres_runs_its_comments_under_the_standard_string_rules() {
        let comment = Change::CreateTable {
            table: table("shop", "t"),
            columns: vec![ColumnSpec { name: "id".into(), data_type: "int".into(), nullable: false, ..ColumnSpec::default() }],
            primary_key: Vec::new(),
            comment: Some("\\'; DROP TABLE t; --".into()),
        };
        // The literal only doubles quotes (the standard rules)...
        assert_eq!(sql(Postgres, comment.clone()).last().unwrap(), "COMMENT ON TABLE \"shop\".\"t\" IS '\\''; DROP TABLE t; --'");
        // ...so the transaction that runs it says it is on, before the first statement.
        let planned = plan(dialect(Postgres), &[comment]);
        assert!(planned.setup.in_transaction.iter().any(|sql| sql == "SET LOCAL standard_conforming_strings = on"));
    }

    #[test]
    fn a_new_name_is_refused_when_it_cannot_be_told_apart_or_is_not_a_name() {
        let d = dialect(Postgres);
        for name in ["", "   ", " padded", "padded ", "line\nbreak", "tab\there", "nul\0"] {
            assert!(checked_name(d, "table", name).is_err(), "{name:?}");
        }
        assert_eq!(checked_name(d, "table", "ok name").unwrap(), "\"ok name\"");
        assert!(checked_name(d, "table", &"a".repeat(64)).unwrap_err().to_string().contains("longer than the 63 bytes"));
        assert!(checked_name(dialect(Mysql), "database", &"é".repeat(65)).is_err());
        // A backslash is refused where a server escapes with it inside the quotes (ClickHouse), and
        // is a plain character where none does.
        let message = checked_name(dialect(ConnectionKind::Clickhouse), "table", "a\\`b").unwrap_err().to_string();
        assert!(message.contains("has a backslash") && message.contains("ClickHouse"), "{message}");
        assert!(checked_name(dialect(ConnectionKind::Clickhouse), "table", "a`b").is_ok());
        for kind in [Postgres, Mysql, Mariadb, Sqlite] {
            assert!(checked_name(dialect(kind), "table", "a\\b").is_ok(), "{kind:?}");
        }
        // Rename and create go through it: the statements refuse the name too.
        let ch = Change::CreateNamespace { kind: NamespaceKind::Database, name: "d\\".into() };
        assert!(refused(ConnectionKind::Clickhouse, ch).contains("has a backslash"));
        let rename = Change::RenameView { table: table("d", "v"), to: "v\\".into(), materialized: false };
        assert!(refused(ConnectionKind::Clickhouse, rename).contains("has a backslash"));
        // The statements refuse them too, not only the dialog.
        let rename = Change::RenameView { table: table("shop", "v"), to: " v2".into(), materialized: false };
        assert!(refused(Postgres, rename).contains("starts or ends with a space"));
        assert!(refused(Postgres, Change::CreateNamespace { kind: NamespaceKind::Schema, name: "a\nb".into() }).contains("control character"));
    }

    #[test]
    fn warnings_name_the_data_loss_of_each_action() {
        let t = table("shop", "orders");
        let warned = |kind: ConnectionKind, change: Change| warnings(dialect(kind), &[change]);
        let destructive = |found: &[Warning]| found.iter().any(|w| w.severity == Severity::Destructive);
        assert!(destructive(&warned(Postgres, drop_table(t.clone(), false))));
        assert!(destructive(&warned(Postgres, drop_view(t.clone(), false, false))));
        let cascade = warned(Postgres, drop_table(t.clone(), true));
        assert!(cascade.iter().any(|w| w.message.contains("CASCADE also drops everything that depends")), "{cascade:?}");
        let truncated = warned(Sqlite, truncate(t.clone(), false, false));
        assert!(destructive(&truncated));
        assert!(
            truncated.iter().any(|w| w.message.contains("has no TRUNCATE")
                && w.message.contains("DELETE FROM")
                && w.message.contains("fires the DELETE triggers")
                && w.message.contains("ON DELETE CASCADE")),
            "{truncated:?}"
        );
        let mysql = warned(Mysql, truncate(t.clone(), false, false));
        assert!(mysql.iter().any(|w| w.message.contains("cannot roll TRUNCATE back")), "{mysql:?}");
        let pg = warned(Postgres, truncate(t.clone(), true, true));
        assert!(pg.iter().any(|w| w.message.contains("RESTART IDENTITY")) && pg.iter().any(|w| w.message.contains("CASCADE also deletes")), "{pg:?}");
        assert!(!destructive(&warned(Postgres, Change::RenameView { table: t, to: "x".into(), materialized: false })));
        assert!(warned(Postgres, Change::CreateNamespace { kind: NamespaceKind::Schema, name: "x".into() }).is_empty());
    }

    #[test]
    fn a_table_comment_is_written_where_the_dialect_has_one_and_refused_where_it_has_none() {
        let t = table("shop", "orders");
        let create = |comment: Option<&str>| Change::CreateTable {
            table: t.clone(),
            columns: vec![ColumnSpec { name: "id".into(), data_type: "int".into(), nullable: false, ..ColumnSpec::default() }],
            primary_key: vec!["id".into()],
            comment: comment.map(str::to_owned),
        };
        let pg = sql(Postgres, create(Some("it's; --")));
        assert_eq!(pg.last().unwrap(), "COMMENT ON TABLE \"shop\".\"orders\" IS 'it''s; --'");
        let mysql = sql(Mysql, create(Some("it's")));
        assert!(mysql[0].ends_with("\n) COMMENT = 'it''s'"), "{}", mysql[0]);
        let ch = sql(ConnectionKind::Clickhouse, create(Some("events")));
        assert!(ch[0].ends_with("ORDER BY (`id`) COMMENT 'events'"), "{}", ch[0]);
        assert!(refused(Sqlite, create(Some("x"))).contains("no table comments"));
        // No comment, no clause; an empty comment is no comment.
        for kind in [Postgres, Mysql, Sqlite] {
            for none in [None, Some("")] {
                assert!(!sql(kind, create(none)).concat().to_uppercase().contains("COMMENT"), "{kind:?}");
            }
        }
        // The change still reads back from a draft file written before the field existed.
        let old: Change = serde_json::from_str(r#"{"op":"create_table","table":{"schema":"shop","name":"t"},"columns":[],"primary_key":[]}"#).unwrap();
        assert!(matches!(old, Change::CreateTable { comment: None, .. }));
    }

    #[test]
    fn plans_wrap_postgres_and_sqlite_in_a_transaction_and_not_mysql() {
        let t = table("shop", "orders");
        for (kind, transactional) in [(Postgres, true), (Sqlite, true), (Mysql, false), (Mariadb, false)] {
            let planned = plan(dialect(kind), &[truncate(t.clone(), false, false)]);
            assert_eq!(planned.transactional, transactional, "{kind:?}");
            assert!(planned.executable());
        }
    }

    #[test]
    fn the_draft_keeps_one_of_each_object_action_and_the_stale_check_follows_the_table() {
        let t = table("shop", "orders");
        let mut draft = Draft::default();
        draft.push(drop_view(t.clone(), false, false));
        draft.push(drop_view(t.clone(), false, false));
        draft.push(truncate(t.clone(), false, false));
        draft.push(Change::CreateNamespace { kind: NamespaceKind::Schema, name: "reports".into() });
        assert_eq!(draft.len(), 3);
        assert!(draft.touches(&t));
        assert_eq!(draft.changes[2].table(), None);
        assert_eq!(draft.changes[2].title(), "Create schema reports");
        assert_eq!(draft.changes[0].title(), "Drop view shop.orders");

        // Truncate and drop need the table; a table that is gone is stale, a namespace never is.
        let mut live = schema::LiveSchema::new(false);
        live.absent(t.clone());
        let found = schema::stale(&[truncate(t.clone(), false, false), drop_table(t.clone(), false)], &live);
        assert_eq!(found.iter().map(|(index, _)| *index).collect::<Vec<_>>(), [0, 1]);
        assert!(schema::stale(&[Change::CreateNamespace { kind: NamespaceKind::Schema, name: "x".into() }], &live).is_empty());
        let mut present = schema::LiveSchema::new(false);
        present.present(t.clone(), schema::LiveTable::default());
        assert!(schema::stale(&[truncate(t.clone(), false, false)], &present).is_empty());
        // The old draft files (no `cascade`) still load.
        let old: Change = serde_json::from_str(r#"{"op":"drop_table","table":{"schema":"shop","name":"orders"}}"#).unwrap();
        assert_eq!(old, drop_table(t, false));
    }
}
