//! Unit tests of the DDL generation: exact SQL per dialect for every kind of
//! change, hostile identifiers, ordering, refusals, warnings, the draft's
//! folding and the apply protocol.

use super::apply::{apply, StatementRunner};
use super::draft::Draft;
use super::schema::{already_applied, stale, LiveSchema, LiveTable};
use super::*;
use crate::connections::ConnectionKind;

fn table(name: &str) -> TableId {
    TableId::new(Some("shop"), name)
}

fn column(name: &str, data_type: &str, nullable: bool) -> ColumnSpec {
    ColumnSpec { name: name.into(), data_type: data_type.into(), nullable, ..ColumnSpec::default() }
}

fn attributes(extra: &str, character_set: Option<(&str, &str)>) -> ColumnAttributes {
    ColumnAttributes {
        character_set: character_set.map(|(set, _)| set.into()),
        collation: character_set.map(|(_, collation)| collation.into()),
        extra: extra.into(),
        generation_expression: None,
    }
}

fn dialect(kind: ConnectionKind) -> &'static dyn DdlDialect {
    dialect_for(kind).expect("a SQL kind")
}

/// The statements of one change.
fn sql(kind: ConnectionKind, change: Change) -> Vec<String> {
    dialect(kind).statements(&change, &[]).expect("generates")
}

fn refusal(kind: ConnectionKind, change: Change) -> String {
    dialect(kind).statements(&change, &[]).expect_err("is refused").to_string()
}

fn fk(name: &str) -> ForeignKeySpec {
    ForeignKeySpec {
        name: name.into(),
        columns: vec!["customer_id".into()],
        ref_table: table("customers"),
        ref_columns: vec!["id".into()],
        on_delete: Some(FkAction::Cascade),
        on_update: None,
    }
}

fn index(name: &str, unique: bool) -> IndexSpec {
    IndexSpec { name: name.into(), columns: vec!["a".into(), "b".into()], unique }
}

fn orders_columns() -> Vec<ColumnSpec> {
    vec![
        ColumnSpec { default: Some("1".into()), ..column("id", "bigint", false) },
        ColumnSpec { comment: Some("who it's for".into()), ..column("customer_id", "bigint", true) },
        ColumnSpec { default: Some("new".into()), ..column("status", "varchar(20)", false) },
    ]
}

// ---- PostgreSQL ---------------------------------------------------------------------------

#[test]
fn postgres_creates_a_table_with_key_defaults_and_comments() {
    let statements = sql(
        ConnectionKind::Postgres,
        Change::CreateTable { table: table("orders"), columns: orders_columns(), primary_key: vec!["id".into()], comment: None },
    );
    assert_eq!(
        statements,
        vec![
            "CREATE TABLE \"shop\".\"orders\" (\n  \"id\" bigint NOT NULL DEFAULT 1,\n  \"customer_id\" bigint,\n  \
             \"status\" varchar(20) NOT NULL DEFAULT 'new',\n  PRIMARY KEY (\"id\")\n)",
            "COMMENT ON COLUMN \"shop\".\"orders\".\"customer_id\" IS 'who it''s for'",
        ]
    );
}

#[test]
fn postgres_column_changes() {
    use ConnectionKind::Postgres as Pg;
    let t = table("orders");
    assert_eq!(
        sql(Pg, Change::AddColumn { table: t.clone(), column: ColumnSpec { comment: Some("note".into()), default: Some("''".into()), ..column("note", "text", false) } }),
        vec![
            "ALTER TABLE \"shop\".\"orders\" ADD COLUMN \"note\" text NOT NULL DEFAULT ''",
            "COMMENT ON COLUMN \"shop\".\"orders\".\"note\" IS 'note'",
        ]
    );
    assert_eq!(
        sql(Pg, Change::DropColumn { table: t.clone(), column: "note".into() }),
        vec!["ALTER TABLE \"shop\".\"orders\" DROP COLUMN \"note\""]
    );
    assert_eq!(
        sql(Pg, Change::RenameColumn { table: t.clone(), from: "note".into(), to: "memo".into() }),
        vec!["ALTER TABLE \"shop\".\"orders\" RENAME COLUMN \"note\" TO \"memo\""]
    );
    let before = ColumnSpec { default: Some("0".into()), ..column("qty", "integer", true) };
    // Type, default, nullability and comment all change: the old default is dropped and put back in
    // the same statement as the type change.
    let after = ColumnSpec { default: Some("1".into()), comment: Some("units".into()), ..column("qty", "bigint", false) };
    assert_eq!(
        sql(Pg, Change::AlterColumn { table: t.clone(), from: before.clone(), to: after }),
        vec![
            "ALTER TABLE \"shop\".\"orders\" ALTER COLUMN \"qty\" DROP DEFAULT, ALTER COLUMN \"qty\" TYPE bigint USING \"qty\"::bigint, \
             ALTER COLUMN \"qty\" SET DEFAULT 1",
            "ALTER TABLE \"shop\".\"orders\" ALTER COLUMN \"qty\" SET NOT NULL",
            "COMMENT ON COLUMN \"shop\".\"orders\".\"qty\" IS 'units'",
        ]
    );
    let only_default = ColumnSpec { default: None, ..before.clone() };
    assert_eq!(
        sql(Pg, Change::AlterColumn { table: t.clone(), from: before.clone(), to: only_default }),
        vec!["ALTER TABLE \"shop\".\"orders\" ALTER COLUMN \"qty\" DROP DEFAULT"]
    );
    let comment_off = ColumnSpec { comment: Some("x".into()), ..before.clone() };
    assert_eq!(
        sql(Pg, Change::AlterColumn { table: t, from: comment_off, to: before }),
        vec!["COMMENT ON COLUMN \"shop\".\"orders\".\"qty\" IS NULL"]
    );
}

#[test]
fn postgres_table_key_index_and_foreign_key_changes() {
    use ConnectionKind::Postgres as Pg;
    let t = table("orders");
    assert_eq!(sql(Pg, Change::DropTable { table: t.clone(), cascade: false }), vec!["DROP TABLE \"shop\".\"orders\""]);
    assert_eq!(
        sql(Pg, Change::RenameTable { table: t.clone(), to: "purchases".into() }),
        vec!["ALTER TABLE \"shop\".\"orders\" RENAME TO \"purchases\""]
    );
    assert_eq!(
        sql(
            Pg,
            Change::SetPrimaryKey {
                table: t.clone(),
                columns: vec!["a".into(), "b".into()],
                previous: Some(PrimaryKeyRef { name: Some("orders_pkey".into()), columns: vec!["id".into()] }),
            }
        ),
        vec![
            "ALTER TABLE \"shop\".\"orders\" DROP CONSTRAINT \"orders_pkey\"",
            "ALTER TABLE \"shop\".\"orders\" ADD PRIMARY KEY (\"a\", \"b\")",
        ]
    );
    assert_eq!(
        sql(Pg, Change::CreateIndex { table: t.clone(), index: index("orders_ab", true) }),
        vec!["CREATE UNIQUE INDEX \"orders_ab\" ON \"shop\".\"orders\" (\"a\", \"b\")"]
    );
    assert_eq!(
        sql(Pg, Change::DropIndex { table: t.clone(), name: "orders_ab".into() }),
        vec!["DROP INDEX \"shop\".\"orders_ab\""]
    );
    assert_eq!(
        sql(Pg, Change::AddForeignKey { table: t.clone(), foreign_key: fk("orders_customer_fk") }),
        vec![
            "ALTER TABLE \"shop\".\"orders\" ADD CONSTRAINT \"orders_customer_fk\" FOREIGN KEY (\"customer_id\") \
             REFERENCES \"shop\".\"customers\" (\"id\") ON DELETE CASCADE"
        ]
    );
    assert_eq!(
        sql(Pg, Change::DropForeignKey { table: t, name: "orders_customer_fk".into() }),
        vec!["ALTER TABLE \"shop\".\"orders\" DROP CONSTRAINT \"orders_customer_fk\""]
    );
}

#[test]
fn postgres_refuses_a_primary_key_it_cannot_name() {
    let error = refusal(
        ConnectionKind::Postgres,
        Change::SetPrimaryKey {
            table: table("orders"),
            columns: vec!["a".into()],
            previous: Some(PrimaryKeyRef { name: None, columns: vec!["id".into()] }),
        },
    );
    assert!(error.contains("name of the current primary key"), "{error}");
}

// ---- MySQL and MariaDB ---------------------------------------------------------------------

#[test]
fn mysql_creates_a_table_and_restates_columns_with_modify() {
    use ConnectionKind::Mysql as My;
    let t = TableId::new(Some("shop"), "orders");
    let mut id = column("id", "bigint", false);
    id.auto_increment = true;
    assert_eq!(
        sql(My, Change::CreateTable { table: t.clone(), columns: vec![id.clone(), orders_columns().remove(1)], primary_key: vec!["id".into()], comment: None }),
        vec![
            "CREATE TABLE `shop`.`orders` (\n  `id` bigint NOT NULL AUTO_INCREMENT,\n  \
             `customer_id` bigint NULL COMMENT 'who it''s for',\n  PRIMARY KEY (`id`)\n)"
        ]
    );
    let before = ColumnSpec { default: Some("0".into()), attributes: Some(attributes("", None)), ..column("qty", "int", true) };
    let after = ColumnSpec { comment: Some("a\\b".into()), ..column("qty", "bigint", false) };
    let after = ColumnSpec { attributes: before.attributes.clone(), default: before.default.clone(), ..after };
    assert_eq!(
        sql(My, Change::AlterColumn { table: t.clone(), from: before, to: after }),
        // The unchanged default is restated as the catalog printed it; backslashes are doubled in the literal.
        vec!["ALTER TABLE `shop`.`orders` MODIFY COLUMN `qty` bigint NOT NULL DEFAULT 0 COMMENT 'a\\\\b'"]
    );
    assert_eq!(
        sql(My, Change::AddColumn { table: t.clone(), column: ColumnSpec { default: Some("now()".into()), ..column("at", "datetime", true) } }),
        vec!["ALTER TABLE `shop`.`orders` ADD COLUMN `at` datetime NULL DEFAULT now()"]
    );
    assert_eq!(
        sql(My, Change::RenameColumn { table: t.clone(), from: "a".into(), to: "b".into() }),
        vec!["ALTER TABLE `shop`.`orders` RENAME COLUMN `a` TO `b`"]
    );
    assert_eq!(
        sql(My, Change::DropColumn { table: t.clone(), column: "a".into() }),
        vec!["ALTER TABLE `shop`.`orders` DROP COLUMN `a`"]
    );
}

#[test]
fn mysql_table_key_index_and_foreign_key_changes() {
    use ConnectionKind::Mariadb as Maria;
    let t = table("orders");
    assert_eq!(
        sql(Maria, Change::RenameTable { table: t.clone(), to: "purchases".into() }),
        vec!["RENAME TABLE `shop`.`orders` TO `shop`.`purchases`"]
    );
    assert_eq!(sql(Maria, Change::DropTable { table: t.clone(), cascade: false }), vec!["DROP TABLE `shop`.`orders`"]);
    assert_eq!(
        sql(
            Maria,
            Change::SetPrimaryKey {
                table: t.clone(),
                columns: vec!["a".into()],
                previous: Some(PrimaryKeyRef { name: Some("PRIMARY".into()), columns: vec!["id".into()] }),
            }
        ),
        vec!["ALTER TABLE `shop`.`orders` DROP PRIMARY KEY, ADD PRIMARY KEY (`a`)"]
    );
    assert_eq!(
        sql(Maria, Change::SetPrimaryKey { table: t.clone(), columns: vec![], previous: Some(PrimaryKeyRef { name: None, columns: vec!["id".into()] }) }),
        vec!["ALTER TABLE `shop`.`orders` DROP PRIMARY KEY"]
    );
    assert_eq!(
        sql(Maria, Change::CreateIndex { table: t.clone(), index: index("ab", false) }),
        vec!["CREATE INDEX `ab` ON `shop`.`orders` (`a`, `b`)"]
    );
    assert_eq!(
        sql(Maria, Change::DropIndex { table: t.clone(), name: "ab".into() }),
        vec!["DROP INDEX `ab` ON `shop`.`orders`"]
    );
    assert_eq!(
        sql(Maria, Change::AddForeignKey { table: t.clone(), foreign_key: fk("fk1") }),
        vec![
            "ALTER TABLE `shop`.`orders` ADD CONSTRAINT `fk1` FOREIGN KEY (`customer_id`) REFERENCES `shop`.`customers` (`id`) ON DELETE CASCADE"
        ]
    );
    assert_eq!(
        sql(Maria, Change::DropForeignKey { table: t.clone(), name: "fk1".into() }),
        vec!["ALTER TABLE `shop`.`orders` DROP FOREIGN KEY `fk1`"]
    );
    let mut set_default = fk("fk2");
    set_default.on_update = Some(FkAction::SetDefault);
    assert!(refusal(Maria, Change::AddForeignKey { table: t, foreign_key: set_default }).contains("SET DEFAULT"));
}

#[test]
fn mysql_restates_every_attribute_of_the_column_and_refuses_what_it_does_not_know() {
    use ConnectionKind::Mysql as My;
    let t = table("orders");
    let alter = |from: ColumnSpec, to: ColumnSpec| Change::AlterColumn { table: t.clone(), from, to };
    // `updated_at timestamp ... ON UPDATE CURRENT_TIMESTAMP(3)`: a comment-only edit keeps it.
    let stamp = ColumnSpec {
        default: Some("CURRENT_TIMESTAMP(3)".into()),
        attributes: Some(attributes("DEFAULT_GENERATED on update CURRENT_TIMESTAMP(3)", None)),
        ..column("updated_at", "timestamp(3)", false)
    };
    let edited = ColumnSpec { comment: Some("last write".into()), ..stamp.clone() };
    assert_eq!(
        sql(My, alter(stamp.clone(), edited)),
        vec!["ALTER TABLE `shop`.`orders` MODIFY COLUMN `updated_at` timestamp(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3) ON UPDATE CURRENT_TIMESTAMP(3) COMMENT 'last write'"]
    );
    // A utf8mb4 column keeps its own character set and collation, auto_increment and invisibility.
    let name = ColumnSpec {
        attributes: Some(attributes("INVISIBLE", Some(("utf8mb4", "utf8mb4_bin")))),
        ..column("name", "varchar(40)", true)
    };
    let widened = ColumnSpec { data_type: "varchar(80)".into(), ..name.clone() };
    assert_eq!(
        sql(My, alter(name.clone(), widened)),
        vec!["ALTER TABLE `shop`.`orders` MODIFY COLUMN `name` varchar(80) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NULL INVISIBLE"]
    );
    let serial = ColumnSpec { attributes: Some(attributes("auto_increment", None)), ..column("id", "int", false) };
    let renamed_type = ColumnSpec { data_type: "bigint".into(), ..serial.clone() };
    assert_eq!(
        sql(My, alter(serial, renamed_type)),
        vec!["ALTER TABLE `shop`.`orders` MODIFY COLUMN `id` bigint NOT NULL AUTO_INCREMENT"]
    );
    // A generated column is restated with its expression.
    let generated = ColumnSpec {
        generated: true,
        attributes: Some(ColumnAttributes { generation_expression: Some("`a` + `b`".into()), ..attributes("STORED GENERATED", None) }),
        ..column("total", "int", true)
    };
    let commented = ColumnSpec { comment: Some("sum".into()), ..generated.clone() };
    assert_eq!(
        sql(My, alter(generated.clone(), commented)),
        vec!["ALTER TABLE `shop`.`orders` MODIFY COLUMN `total` int GENERATED ALWAYS AS (`a` + `b`) STORED NULL COMMENT 'sum'"]
    );
    // Refused: an older driver that reported nothing, an EXTRA nobody understands, a string column without a charset,
    // a generated column whose expression is unknown.
    let change = |from: ColumnSpec| alter(from.clone(), ColumnSpec { comment: Some("x".into()), ..from });
    let old_driver = column("qty", "int", true);
    assert!(refusal(My, change(old_driver)).contains("did not report"));
    let odd = ColumnSpec { attributes: Some(attributes("SRID 4326 hypothetical", None)), ..column("g", "int", true) };
    assert!(refusal(My, change(odd)).contains("EXTRA"));
    let no_charset = ColumnSpec { attributes: Some(attributes("", None)), ..column("s", "varchar(5)", true) };
    assert!(refusal(My, change(no_charset)).contains("character set"));
    let no_expression = ColumnSpec { generated: true, attributes: Some(attributes("VIRTUAL GENERATED", None)), ..column("t", "int", true) };
    assert!(refusal(My, change(no_expression)).contains("expression is not known"));
    // Nothing changes, nothing is generated (even for a column that could not be restated).
    let same = column("qty", "int", true);
    assert_eq!(sql(My, alter(same.clone(), same)), Vec::<String>::new());
}

// ---- SQLite --------------------------------------------------------------------------------

#[test]
fn sqlite_creates_tables_with_inline_foreign_keys_and_alters_what_it_can() {
    use ConnectionKind::Sqlite as Lite;
    let t = TableId::new(Some("main"), "orders");
    let planned = plan(
        dialect(Lite),
        &[
            Change::AddForeignKey { table: t.clone(), foreign_key: ForeignKeySpec { ref_table: TableId::new(Some("main"), "customers"), ..fk("fk1") } },
            Change::CreateTable {
                table: t.clone(),
                columns: vec![column("id", "INTEGER", false), column("customer_id", "INTEGER", true)],
                primary_key: vec!["id".into()],
                comment: None,
            },
        ],
    );
    assert!(planned.executable());
    let created = planned.steps();
    assert_eq!(
        created.iter().map(|s| s.sql.as_str()).collect::<Vec<_>>(),
        vec![
            "CREATE TABLE \"main\".\"orders\" (\n  \"id\" INTEGER NOT NULL,\n  \"customer_id\" INTEGER,\n  PRIMARY KEY (\"id\"),\n  \
             CONSTRAINT \"fk1\" FOREIGN KEY (\"customer_id\") REFERENCES \"customers\" (\"id\") ON DELETE CASCADE\n)"
        ]
    );
    assert_eq!(planned.entry(0).and_then(|e| e.folded_into), Some(1), "the foreign key is part of CREATE TABLE");

    assert_eq!(
        sql(Lite, Change::AddColumn { table: t.clone(), column: ColumnSpec { default: Some("0".into()), ..column("qty", "INTEGER", false) } }),
        vec!["ALTER TABLE \"main\".\"orders\" ADD COLUMN \"qty\" INTEGER NOT NULL DEFAULT 0"]
    );
    assert_eq!(
        sql(Lite, Change::RenameColumn { table: t.clone(), from: "a".into(), to: "b".into() }),
        vec!["ALTER TABLE \"main\".\"orders\" RENAME COLUMN \"a\" TO \"b\""]
    );
    assert_eq!(
        sql(Lite, Change::DropColumn { table: t.clone(), column: "a".into() }),
        vec!["ALTER TABLE \"main\".\"orders\" DROP COLUMN \"a\""]
    );
    assert_eq!(
        sql(Lite, Change::RenameTable { table: t.clone(), to: "sales".into() }),
        vec!["ALTER TABLE \"main\".\"orders\" RENAME TO \"sales\""]
    );
    assert_eq!(
        sql(Lite, Change::CreateIndex { table: t.clone(), index: index("ab", true) }),
        vec!["CREATE UNIQUE INDEX \"main\".\"ab\" ON \"orders\" (\"a\", \"b\")"]
    );
    assert_eq!(sql(Lite, Change::DropIndex { table: t.clone(), name: "ab".into() }), vec!["DROP INDEX \"main\".\"ab\""]);
    assert_eq!(sql(Lite, Change::DropTable { table: t, cascade: false }), vec!["DROP TABLE \"main\".\"orders\""]);
}

#[test]
fn sqlite_refuses_what_needs_a_table_rebuild() {
    use ConnectionKind::Sqlite as Lite;
    let t = table("orders");
    let alter = Change::AlterColumn { table: t.clone(), from: column("a", "INTEGER", true), to: column("a", "TEXT", true) };
    assert!(refusal(Lite, alter).contains("SQLite cannot change the type, nullability, default or comment"));
    assert!(refusal(Lite, Change::SetPrimaryKey { table: t.clone(), columns: vec!["a".into()], previous: None })
        .contains("without rebuilding the table"));
    assert!(refusal(Lite, Change::AddForeignKey { table: t.clone(), foreign_key: fk("fk") }).contains("existing table"));
    assert!(refusal(Lite, Change::DropForeignKey { table: t.clone(), name: "fk".into() }).contains("cannot drop the foreign key"));
    assert!(refusal(Lite, Change::AddColumn { table: t.clone(), column: column("n", "INTEGER", false) }).contains("NOT NULL"));
    let commented = ColumnSpec { comment: Some("x".into()), ..column("n", "INTEGER", true) };
    assert!(refusal(Lite, Change::AddColumn { table: t, column: commented }).contains("no column comments"));
}

// ---- ClickHouse ----------------------------------------------------------------------------

#[test]
fn clickhouse_writes_nullability_into_the_type() {
    use ConnectionKind::Clickhouse as Ch;
    let t = TableId::new(Some("shop"), "events");
    assert_eq!(
        sql(
            Ch,
            Change::CreateTable {
                table: t.clone(),
                columns: vec![column("id", "UInt64", false), ColumnSpec { comment: Some("who".into()), ..column("user", "String", true) }],
                primary_key: vec!["id".into()],
                comment: None,
            }
        ),
        vec![
            "CREATE TABLE `shop`.`events` (\n  `id` UInt64,\n  `user` Nullable(String) COMMENT 'who'\n) ENGINE = MergeTree ORDER BY (`id`)"
        ]
    );
    assert_eq!(
        sql(Ch, Change::CreateTable { table: t.clone(), columns: vec![column("v", "Int32", false)], primary_key: vec![], comment: None }),
        vec!["CREATE TABLE `shop`.`events` (\n  `v` Int32\n) ENGINE = MergeTree ORDER BY tuple()"]
    );
    assert_eq!(
        sql(Ch, Change::AddColumn { table: t.clone(), column: ColumnSpec { default: Some("0".into()), ..column("n", "Int32", false) } }),
        vec!["ALTER TABLE `shop`.`events` ADD COLUMN `n` Int32 DEFAULT 0"]
    );
    // Nullable(String) -> NOT NULL needs a default (ClickHouse refuses the conversion otherwise).
    let no_default = Change::AlterColumn {
        table: t.clone(),
        from: column("user", "Nullable(String)", true),
        to: column("user", "Nullable(String)", false),
    };
    assert!(refusal(Ch, no_default).contains("needs a default"));
    // Nullable(String) -> NOT NULL unwraps it; a new default and comment follow.
    let before = column("user", "Nullable(String)", true);
    let after = ColumnSpec { default: Some("anon".into()), comment: Some("c".into()), ..column("user", "Nullable(String)", false) };
    assert_eq!(
        sql(Ch, Change::AlterColumn { table: t.clone(), from: before, to: after }),
        vec![
            "ALTER TABLE `shop`.`events` MODIFY COLUMN `user` String DEFAULT 'anon'",
            "ALTER TABLE `shop`.`events` COMMENT COLUMN `user` 'c'",
        ]
    );
    let had_default = ColumnSpec { default: Some("0".into()), ..column("n", "Int32", false) };
    assert_eq!(
        sql(Ch, Change::AlterColumn { table: t.clone(), from: had_default, to: column("n", "Int32", false) }),
        vec!["ALTER TABLE `shop`.`events` MODIFY COLUMN `n` REMOVE DEFAULT"]
    );
    assert_eq!(
        sql(Ch, Change::RenameTable { table: t.clone(), to: "log".into() }),
        vec!["RENAME TABLE `shop`.`events` TO `shop`.`log`"]
    );
    assert_eq!(
        sql(Ch, Change::RenameColumn { table: t.clone(), from: "a".into(), to: "b".into() }),
        vec!["ALTER TABLE `shop`.`events` RENAME COLUMN `a` TO `b`"]
    );
    assert_eq!(sql(Ch, Change::DropColumn { table: t.clone(), column: "a".into() }), vec!["ALTER TABLE `shop`.`events` DROP COLUMN `a`"]);
    assert_eq!(sql(Ch, Change::DropTable { table: t, cascade: false }), vec!["DROP TABLE `shop`.`events`"]);
}

#[test]
fn clickhouse_refuses_keys_indexes_and_foreign_keys() {
    use ConnectionKind::Clickhouse as Ch;
    let t = table("events");
    assert!(refusal(Ch, Change::AddForeignKey { table: t.clone(), foreign_key: fk("fk") }).contains("no foreign keys"));
    assert!(refusal(Ch, Change::DropForeignKey { table: t.clone(), name: "fk".into() }).contains("no foreign keys"));
    assert!(refusal(Ch, Change::CreateIndex { table: t.clone(), index: index("i", false) }).contains("secondary indexes"));
    assert!(refusal(Ch, Change::DropIndex { table: t.clone(), name: "i".into() }).contains("secondary indexes"));
    assert!(refusal(Ch, Change::SetPrimaryKey { table: t.clone(), columns: vec!["a".into()], previous: None }).contains("sorting key"));
    let error = refusal(
        Ch,
        Change::CreateTable { table: t, columns: vec![column("id", "UInt64", true)], primary_key: vec!["id".into()], comment: None },
    );
    assert!(error.contains("cannot be Nullable"), "{error}");
}

// ---- identifiers, literals, free text -------------------------------------------------------

#[test]
fn hostile_identifiers_are_quoted_in_every_dialect() {
    let names = ["we\"ird", "back`tick", "sp ace", "ünï©ode✓", "semi;colon--x", "'; DROP TABLE t; --"];
    for kind in [ConnectionKind::Postgres, ConnectionKind::Mysql, ConnectionKind::Mariadb, ConnectionKind::Sqlite, ConnectionKind::Clickhouse] {
        let d = dialect(kind);
        let quote = d.quoting().identifier_quote;
        for name in names {
            let quoted = d.quoting().quote(name);
            let t = TableId::new(Some(name), name);
            let statements = d
                .statements(&Change::RenameColumn { table: t.clone(), from: name.into(), to: format!("{name}2") }, &[])
                .expect("renames");
            let expected_table = format!("{}.{}", quoted, quoted);
            assert!(statements[0].contains(&expected_table), "{kind:?} {name}: {}", statements[0]);
            assert!(statements[0].contains(&quoted), "{kind:?} {name}");
            // Every embedded quote character is doubled, so the name stays one token.
            let doubled = name.replace(quote, &format!("{quote}{quote}"));
            assert!(statements[0].contains(&format!("{quote}{doubled}{quote}")), "{kind:?} {name}: {}", statements[0]);
            let dropped = d.statements(&Change::DropTable { table: t, cascade: false }, &[]).expect("drops");
            assert_eq!(dropped, vec![format!("DROP TABLE {expected_table}")], "{kind:?} {name}");
        }
    }
}

#[test]
fn hostile_comments_and_defaults_cannot_break_out_of_their_literal() {
    let pg = dialect(ConnectionKind::Postgres);
    let my = dialect(ConnectionKind::Mysql);
    let evil = "x'); DROP TABLE users; --";
    // As a default the text is a literal (no parenthesis opens it), so the quote is doubled...
    let commented = ColumnSpec { comment: Some(evil.into()), default: Some(evil.into()), ..column("c", "text", true) };
    assert_eq!(
        pg.statements(&Change::AddColumn { table: table("t"), column: commented }, &[]).expect("pg")[0],
        "ALTER TABLE \"shop\".\"t\" ADD COLUMN \"c\" text DEFAULT 'x''); DROP TABLE users; --'"
    );
    // ...and a default that does read as an expression is refused when it carries a second statement.
    let expression = ColumnSpec { default: Some("now(); DROP TABLE users".into()), ..column("c", "text", true) };
    assert!(pg.statements(&Change::AddColumn { table: table("t"), column: expression }, &[]).is_err());
    let plain_default = ColumnSpec { default: Some("it's".into()), comment: Some(evil.into()), ..column("c", "text", true) };
    assert_eq!(
        pg.statements(&Change::AddColumn { table: table("t"), column: plain_default.clone() }, &[]).expect("pg"),
        vec![
            "ALTER TABLE \"shop\".\"t\" ADD COLUMN \"c\" text DEFAULT 'it''s'",
            "COMMENT ON COLUMN \"shop\".\"t\".\"c\" IS 'x''); DROP TABLE users; --'",
        ]
    );
    assert_eq!(
        my.statements(&Change::AddColumn { table: table("t"), column: plain_default }, &[]).expect("mysql"),
        vec!["ALTER TABLE `shop`.`t` ADD COLUMN `c` text NULL DEFAULT 'it''s' COMMENT 'x''); DROP TABLE users; --'"]
    );
}

#[test]
fn types_and_defaults_are_checked_before_they_reach_a_statement() {
    let pg = dialect(ConnectionKind::Postgres);
    for bad_type in ["", "  ", "int; DROP TABLE t", "int -- x", "varchar(10", "int /* x */"] {
        let error = pg
            .statements(&Change::AddColumn { table: table("t"), column: column("c", bad_type, true) }, &[])
            .expect_err(bad_type);
        assert!(matches!(error, DdlError::Invalid(_)), "{bad_type}");
    }
    for ok_type in ["integer", "varchar(255)", "numeric(10, 2)", "timestamp with time zone", "int[]"] {
        assert!(pg.statements(&Change::AddColumn { table: table("t"), column: column("c", ok_type, true) }, &[]).is_ok(), "{ok_type}");
    }
    assert_eq!(default_sql(pg, "42").unwrap(), "42");
    assert_eq!(default_sql(pg, "-1.5e3").unwrap(), "-1.5e3");
    assert_eq!(default_sql(pg, "null").unwrap(), "null");
    assert_eq!(default_sql(pg, "CURRENT_TIMESTAMP").unwrap(), "CURRENT_TIMESTAMP");
    assert_eq!(default_sql(pg, "now()").unwrap(), "now()");
    assert_eq!(default_sql(pg, "'a'::text").unwrap(), "'a'::text");
    assert_eq!(default_sql(pg, "nextval('s'::regclass)").unwrap(), "nextval('s'::regclass)");
    assert_eq!(default_sql(pg, "active").unwrap(), "'active'");
    assert_eq!(default_sql(pg, "O'Brien").unwrap(), "'O''Brien'");
    assert!(default_sql(pg, "now(); DROP TABLE t").is_err());
    assert!(default_sql(pg, "now() -- x").is_err());
    assert!(default_sql(pg, "(1").is_err());
}

#[test]
fn identifiers_are_validated() {
    let pg = dialect(ConnectionKind::Postgres);
    let blank = pg.statements(&Change::DropColumn { table: table("t"), column: " ".into() }, &[]).unwrap_err();
    assert!(blank.to_string().contains("column name is empty"), "{blank}");
    let long = "a".repeat(64);
    let error = pg.statements(&Change::DropTable { table: TableId::new(None, &long), cascade: false }, &[]).unwrap_err();
    assert!(error.to_string().contains("63 bytes"), "{error}");
    assert!(pg.statements(&Change::DropTable { table: TableId::new(None, &"a".repeat(63)), cascade: false }, &[]).is_ok());
    let nul = pg.statements(&Change::DropTable { table: TableId::new(None, "a\0b"), cascade: false }, &[]).unwrap_err();
    assert!(nul.to_string().contains("NUL"), "{nul}");
    assert!(dialect(ConnectionKind::Mysql).statements(&Change::DropTable { table: TableId::new(None, &"é".repeat(64)), cascade: false }, &[]).is_ok());
    assert!(dialect(ConnectionKind::Mysql).statements(&Change::DropTable { table: TableId::new(None, &"é".repeat(65)), cascade: false }, &[]).is_err());
}

#[test]
fn a_foreign_key_needs_matching_column_lists() {
    let mut broken = fk("fk");
    broken.ref_columns.push("other".into());
    let error = refusal(ConnectionKind::Postgres, Change::AddForeignKey { table: table("t"), foreign_key: broken });
    assert!(error.contains("references"), "{error}");
}

// ---- ordering ------------------------------------------------------------------------------

fn order_of(planned: &Planned) -> Vec<usize> {
    planned.entries.iter().map(|e| e.change).collect()
}

#[test]
fn foreign_keys_are_dropped_before_their_column_and_created_after_their_table() {
    let t = table("orders");
    let changes = vec![
        Change::AddForeignKey { table: t.clone(), foreign_key: fk("new_fk") },
        Change::DropColumn { table: t.clone(), column: "customer_id".into() },
        Change::CreateTable { table: table("customers"), columns: vec![column("id", "bigint", false)], primary_key: vec!["id".into()], comment: None },
        Change::DropForeignKey { table: t.clone(), name: "old_fk".into() },
        Change::AddColumn { table: t.clone(), column: column("note", "text", true) },
        Change::DropIndex { table: t.clone(), name: "old_idx".into() },
        Change::CreateIndex { table: t.clone(), index: index("new_idx", false) },
        Change::RenameTable { table: table("legacy"), to: "archive".into() },
        Change::DropTable { table: table("gone"), cascade: false },
        Change::SetPrimaryKey { table: t.clone(), columns: vec!["id".into()], previous: None },
        Change::AlterColumn { table: t, from: column("qty", "int", true), to: column("qty", "bigint", true) },
        Change::RenameColumn { table: table("orders"), from: "a".into(), to: "b".into() },
    ];
    let planned = plan(dialect(ConnectionKind::Postgres), &changes);
    assert!(planned.executable());
    // rename table, drop fk, drop index, drop column, drop table, create table, rename column,
    // add column, alter column, primary key, create index, add fk
    assert_eq!(order_of(&planned), vec![7, 3, 5, 1, 8, 2, 11, 4, 10, 9, 6, 0]);
    let script = planned.script();
    let at = |needle: &str| script.find(needle).unwrap_or_else(|| panic!("{needle} in {script}"));
    assert!(at("DROP CONSTRAINT \"old_fk\"") < at("DROP COLUMN \"customer_id\""));
    assert!(at("CREATE TABLE \"shop\".\"customers\"") < at("ADD CONSTRAINT \"new_fk\""));
    assert!(script.ends_with(';'));
}

#[test]
fn changes_that_cannot_be_generated_are_reported_per_change() {
    let t = table("orders");
    let changes = vec![
        Change::AddColumn { table: t.clone(), column: column("ok", "text", true) },
        Change::AddForeignKey { table: t, foreign_key: fk("fk") },
    ];
    let planned = plan(dialect(ConnectionKind::Clickhouse), &changes);
    assert!(!planned.executable());
    let (index, error) = planned.first_error().expect("an error");
    assert_eq!(index, 1);
    assert!(error.to_string().contains("foreign keys"));
    assert_eq!(planned.steps().len(), 1, "the other change still has its SQL");
}

// ---- warnings ------------------------------------------------------------------------------

#[test]
fn data_loss_is_flagged_as_destructive() {
    let t = table("orders");
    let pg = dialect(ConnectionKind::Postgres);
    let changes = vec![
        Change::DropTable { table: t.clone(), cascade: false },
        Change::DropColumn { table: t.clone(), column: "note".into() },
        Change::AlterColumn { table: t.clone(), from: column("n", "bigint", true), to: column("n", "integer", true) },
        Change::AlterColumn { table: t.clone(), from: column("s", "varchar(100)", true), to: column("s", "varchar(20)", true) },
        Change::AlterColumn { table: t.clone(), from: column("w", "integer", true), to: column("w", "bigint", true) },
        Change::AlterColumn { table: t.clone(), from: column("z", "integer", true), to: column("z", "integer", false) },
        Change::RenameColumn { table: t.clone(), from: "a".into(), to: "b".into() },
        Change::AlterColumn { table: t.clone(), from: column("d", "timestamp", true), to: column("d", "date", true) },
    ];
    let found = warnings(pg, &changes);
    let severity = |change: usize| found.iter().filter(|w| w.change == change).map(|w| w.severity).max();
    assert_eq!(severity(0), Some(Severity::Destructive));
    assert_eq!(severity(1), Some(Severity::Destructive));
    assert_eq!(severity(2), Some(Severity::Destructive), "bigint -> integer narrows");
    assert_eq!(severity(3), Some(Severity::Destructive), "varchar(100) -> varchar(20) narrows");
    assert_eq!(severity(4), Some(Severity::Caution), "integer -> bigint is a proven widening");
    assert_eq!(severity(5), Some(Severity::Caution), "PostgreSQL: NULL -> NOT NULL fails if there are NULLs");
    assert_eq!(severity(6), Some(Severity::Caution));
    assert_eq!(severity(7), Some(Severity::Destructive), "timestamp -> date drops the time of day");
    // On MySQL, MariaDB and ClickHouse the NULLs are rewritten instead: destructive.
    let not_null = [Change::AlterColumn { table: t, from: column("z", "integer", true), to: column("z", "integer", false) }];
    for (kind, expected) in [
        (ConnectionKind::Postgres, Severity::Caution),
        (ConnectionKind::Sqlite, Severity::Caution),
        (ConnectionKind::Mysql, Severity::Destructive),
        (ConnectionKind::Mariadb, Severity::Destructive),
        (ConnectionKind::Clickhouse, Severity::Destructive),
    ] {
        let found = warnings(dialect(kind), &not_null);
        assert_eq!(found.iter().map(|w| w.severity).max(), Some(expected), "{kind:?}");
    }
}

#[test]
fn the_review_classifies_a_type_change_by_the_servers_reading_of_the_type() {
    let t = table("t");
    let change = |from: &str, to: &str| [Change::AlterColumn { table: t.clone(), from: column("c", from, true), to: column("c", to, true) }];
    let severity = |kind, from: &str, to: &str| warnings(dialect(kind), &change(from, to)).iter().map(|w| w.severity).max();
    for (kind, from, to, expected) in [
        (ConnectionKind::Mysql, "decimal(12,2)", "decimal", Some(Severity::Destructive)),
        (ConnectionKind::Mariadb, "bigint", "decimal", Some(Severity::Destructive)),
        (ConnectionKind::Postgres, "decimal(12,2)", "decimal", Some(Severity::Caution)),
        (ConnectionKind::Postgres, "text", "varchar(500)", Some(Severity::Destructive)),
        (ConnectionKind::Mysql, "datetime", "timestamp", Some(Severity::Destructive)),
        (ConnectionKind::Postgres, "timestamp", "timestamp(6)", Some(Severity::Caution)),
    ] {
        assert_eq!(severity(kind, from, to), expected, "{kind:?}: {from} -> {to}");
    }
}

#[test]
fn only_a_proven_widening_is_not_destructive() {
    use Flavor::{ClickHouse as CH, MySql as MY, Postgres as PG};
    // (server, from, to, a pure widening)
    for (flavor, from, to, widening) in [
        // The cases that lose data and were once missed.
        (PG, "timestamp", "date", false),
        (PG, "timestamp without time zone", "date", false),
        (PG, "double precision", "integer", false),
        (MY, "float", "int", false),
        (PG, "float8", "float4", false),
        (MY, "double", "float", false),
        (MY, "datetime(6)", "datetime", false),
        (PG, "timestamp(6)", "timestamp(0)", false),
        (MY, "int unsigned", "int", false),
        (MY, "int", "int unsigned", false),
        (PG, "bigint", "integer", false),
        (PG, "varchar(255)", "varchar(20)", false),
        (PG, "text", "varchar(10)", false),
        (PG, "varchar(10)", "char(10)", false),
        (PG, "numeric(12,2)", "numeric(8,2)", false),
        (PG, "numeric(12,4)", "numeric(12,2)", false),
        (PG, "numeric(10,2)", "numeric(11,4)", false),
        (PG, "numeric(10,2)", "integer", false),
        (PG, "text", "integer", false),
        (PG, "integer", "double precision", false),
        (PG, "json", "jsonb", false),
        (PG, "timestamp without time zone", "timestamp with time zone", false),
        (CH, "Nullable(Int64)", "Nullable(Int32)", false),
        (CH, "Int64", "Int8", false),
        (PG, "jsonb", "text", false),
        // The same text, read differently by each server.
        (MY, "decimal(12,2)", "decimal", false), // a bare MySQL decimal is decimal(10,0)
        (MY, "bigint", "decimal", false),
        (MY, "bigint", "numeric", false),
        (MY, "decimal(8,2)", "decimal", false),
        (PG, "decimal(12,2)", "decimal", true), // PostgreSQL's is unconstrained
        (PG, "bigint", "numeric", true),
        (PG, "text", "varchar(10000)", false), // PostgreSQL's text is unbounded
        (PG, "text", "character varying(255)", false),
        (MY, "datetime", "timestamp", false), // timestamp only covers 1970 to 2038
        (MY, "datetime(6)", "timestamp(6)", false),
        (MY, "timestamp", "datetime", false),
        (MY, "date", "timestamp", false),
        (MY, "datetime(6)", "datetime", false),
        (CH, "Date", "DateTime", false),
        // Allowlisted widenings.
        (MY, "int", "decimal", true), // decimal(10,0) holds every int
        (MY, "decimal(8,2)", "decimal(12,2)", true),
        (MY, "varchar(100)", "text", true),
        (MY, "datetime", "datetime(6)", true), // a bare MySQL datetime has no fractional digits
        (MY, "time", "time(3)", true),
        (MY, "date", "datetime", true),
        (PG, "date", "timestamp", true),
        (PG, "timestamp", "timestamp(6)", true),
        (PG, "varchar(10)", "text", true),
        (MY, "int", "bigint", true),
        (PG, "integer", "bigint", true),
        (PG, "smallint", "integer", true),
        (MY, "int unsigned", "bigint", true),
        (MY, "int unsigned", "bigint unsigned", true),
        (MY, "tinyint", "int", true),
        (PG, "varchar(20)", "varchar(255)", true),
        (PG, "character varying(50)", "character varying(80)", true),
        (PG, "varchar(10)", "text", true),
        (PG, "char(10)", "varchar(20)", true),
        (PG, "char(10)", "char(12)", true),
        (MY, "text", "longtext", true),
        (PG, "numeric(8,2)", "numeric(12,2)", true),
        (PG, "numeric(8,2)", "numeric(12,4)", true),
        (PG, "numeric(10,2)", "numeric", true),
        (PG, "integer", "numeric(12,2)", true),
        (PG, "real", "double precision", true),
        (MY, "float", "double", true),
        (PG, "float4", "float8", true),
        (CH, "Float32", "Float64", true),
        (CH, "Nullable(Int32)", "Nullable(Int64)", true),
        (CH, "UInt32", "UInt64", true),
        (PG, "date", "timestamp without time zone", true),
        (MY, "date", "datetime", true),
        (MY, "datetime(3)", "datetime(6)", true),
        (PG, "timestamp(3)", "timestamp(6)", true),
        (PG, "text", "text", true),
        (PG, "varchar(20)", "VARCHAR(20)", true),
    ] {
        assert_eq!(widens(flavor, from, to), widening, "{flavor:?}: {from} -> {to}");
    }
}

// ---- the draft -----------------------------------------------------------------------------

fn pending(draft: &Draft) -> Vec<String> {
    draft.changes.iter().map(Change::title).collect()
}

#[test]
fn editing_a_column_twice_is_one_change_and_undoing_it_removes_it() {
    let t = table("orders");
    let base = column("qty", "integer", true);
    let mut draft = Draft::default();
    draft.push(Change::AlterColumn { table: t.clone(), from: base.clone(), to: column("qty", "bigint", true) });
    draft.push(Change::AlterColumn { table: t.clone(), from: column("qty", "bigint", true), to: column("qty", "bigint", false) });
    assert_eq!(draft.len(), 1);
    assert_eq!(
        draft.changes[0],
        Change::AlterColumn { table: t.clone(), from: base.clone(), to: column("qty", "bigint", false) }
    );
    draft.push(Change::AlterColumn { table: t.clone(), from: column("qty", "bigint", false), to: base });
    assert!(draft.is_empty());
}

#[test]
fn adding_then_dropping_leaves_nothing_and_dropping_an_existing_column_stays() {
    let t = table("orders");
    let mut draft = Draft::default();
    draft.push(Change::AddColumn { table: t.clone(), column: column("note", "text", true) });
    draft.push(Change::CreateIndex { table: t.clone(), index: IndexSpec { name: "n".into(), columns: vec!["note".into()], unique: false } });
    draft.push(Change::DropColumn { table: t.clone(), column: "note".into() });
    assert!(draft.is_empty(), "{:?}", pending(&draft));
    draft.push(Change::DropColumn { table: t.clone(), column: "old".into() });
    assert_eq!(pending(&draft), vec!["Drop column shop.orders.old"]);
    // Discarding it brings the column back.
    assert!(draft.discard(0));
    assert!(draft.is_empty());
    assert!(!draft.discard(0));
}

#[test]
fn renames_rewrite_what_refers_to_them_and_renaming_back_removes_them() {
    let t = table("orders");
    let mut draft = Draft::default();
    draft.push(Change::AddColumn { table: t.clone(), column: column("note", "text", true) });
    draft.push(Change::AlterColumn { table: t.clone(), from: column("qty", "integer", true), to: column("qty", "bigint", true) });
    draft.push(Change::RenameColumn { table: t.clone(), from: "qty".into(), to: "amount".into() });
    draft.push(Change::RenameColumn { table: t.clone(), from: "note".into(), to: "memo".into() });
    assert_eq!(
        draft.changes,
        vec![
            Change::AddColumn { table: t.clone(), column: column("memo", "text", true) },
            Change::AlterColumn { table: t.clone(), from: column("amount", "integer", true), to: column("amount", "bigint", true) },
            Change::RenameColumn { table: t.clone(), from: "qty".into(), to: "amount".into() },
        ]
    );
    // amount -> total merges with qty -> amount; total -> qty cancels.
    draft.push(Change::RenameColumn { table: t.clone(), from: "amount".into(), to: "total".into() });
    assert!(matches!(&draft.changes[2], Change::RenameColumn { from, to, .. } if from == "qty" && to == "total"));
    draft.push(Change::RenameColumn { table: t.clone(), from: "total".into(), to: "qty".into() });
    assert!(!draft.changes.iter().any(|c| matches!(c, Change::RenameColumn { .. })), "{:?}", pending(&draft));

    // A table rename rewrites the other changes and the foreign keys pointing at the table.
    let mut draft = Draft::default();
    draft.push(Change::AddColumn { table: t.clone(), column: column("note", "text", true) });
    draft.push(Change::AddForeignKey { table: table("lines"), foreign_key: ForeignKeySpec { ref_table: t.clone(), ..fk("fk") } });
    draft.push(Change::RenameTable { table: t.clone(), to: "sales".into() });
    let renamed = table("sales");
    assert_eq!(draft.changes[0].table(), Some(&renamed));
    assert!(matches!(&draft.changes[1], Change::AddForeignKey { foreign_key, .. } if foreign_key.ref_table == renamed));
    assert!(matches!(&draft.changes[2], Change::RenameTable { table, to } if *table == t && to == "sales"));
    // Discarding the rename puts the old names back.
    assert!(draft.discard(2));
    assert_eq!(draft.changes[0].table(), Some(&t));
    // Rename and rename back cancel.
    draft.push(Change::RenameTable { table: t.clone(), to: "sales".into() });
    draft.push(Change::RenameTable { table: renamed, to: "orders".into() });
    assert!(!draft.changes.iter().any(|c| matches!(c, Change::RenameTable { .. })));
}

#[test]
fn a_table_created_in_the_draft_absorbs_its_edits() {
    let t = table("tags");
    let mut draft = Draft::default();
    draft.push(Change::CreateTable { table: t.clone(), columns: vec![column("id", "bigint", false)], primary_key: vec!["id".into()], comment: None });
    draft.push(Change::AddColumn { table: t.clone(), column: column("label", "text", true) });
    draft.push(Change::AlterColumn { table: t.clone(), from: column("label", "text", true), to: column("label", "varchar(40)", false) });
    draft.push(Change::RenameColumn { table: t.clone(), from: "label".into(), to: "name".into() });
    draft.push(Change::SetPrimaryKey { table: t.clone(), columns: vec!["id".into(), "name".into()], previous: None });
    draft.push(Change::RenameTable { table: t.clone(), to: "labels".into() });
    let labels = table("labels");
    assert_eq!(
        draft.changes,
        vec![Change::CreateTable {
            table: labels.clone(),
            columns: vec![column("id", "bigint", false), column("name", "varchar(40)", false)],
            primary_key: vec!["id".into(), "name".into()],
            comment: None,
        }]
    );
    draft.push(Change::AddForeignKey { table: labels.clone(), foreign_key: fk("fk") });
    draft.push(Change::AddForeignKey { table: table("lines"), foreign_key: ForeignKeySpec { ref_table: labels.clone(), ..fk("fk2") } });
    draft.push(Change::DropTable { table: labels, cascade: false });
    assert!(draft.is_empty(), "{:?}", pending(&draft));
}

#[test]
fn dropping_a_renamed_table_drops_it_under_its_database_name() {
    let t = table("orders");
    let mut draft = Draft::default();
    draft.push(Change::RenameTable { table: t.clone(), to: "sales".into() });
    draft.push(Change::AddColumn { table: table("sales"), column: column("n", "text", true) });
    draft.push(Change::DropTable { table: table("sales"), cascade: false });
    assert_eq!(draft.changes, vec![Change::DropTable { table: t, cascade: false }]);
}

#[test]
fn indexes_and_foreign_keys_pair_up_with_their_drops() {
    let t = table("orders");
    let mut draft = Draft::default();
    draft.push(Change::CreateIndex { table: t.clone(), index: index("i", false) });
    draft.push(Change::DropIndex { table: t.clone(), name: "i".into() });
    draft.push(Change::AddForeignKey { table: t.clone(), foreign_key: fk("f") });
    draft.push(Change::DropForeignKey { table: t.clone(), name: "f".into() });
    assert!(draft.is_empty());
    draft.push(Change::DropIndex { table: t.clone(), name: "old".into() });
    draft.push(Change::DropIndex { table: t.clone(), name: "old".into() });
    draft.push(Change::SetPrimaryKey { table: t.clone(), columns: vec!["b".into()], previous: Some(PrimaryKeyRef { name: Some("pk".into()), columns: vec!["a".into()] }) });
    draft.push(Change::SetPrimaryKey { table: t.clone(), columns: vec!["a".into()], previous: Some(PrimaryKeyRef { name: Some("pk".into()), columns: vec!["b".into()] }) });
    assert_eq!(draft.len(), 1, "back to the original key: {:?}", pending(&draft));
}

#[test]
fn a_draft_round_trips_through_json() {
    let mut draft = Draft::default();
    draft.push(Change::CreateTable { table: table("t"), columns: orders_columns(), primary_key: vec!["id".into()], comment: None });
    draft.push(Change::AddForeignKey { table: table("t"), foreign_key: fk("f") });
    draft.push(Change::DropIndex { table: table("t"), name: "i".into() });
    let json = serde_json::to_string(&draft.changes).unwrap();
    let back: Vec<Change> = serde_json::from_str(&json).unwrap();
    assert_eq!(back, draft.changes);
    assert!(json.contains("\"op\":\"create_table\""), "{json}");
}

#[test]
fn applied_changes_leave_the_draft() {
    let mut draft = Draft::default();
    draft.push(Change::DropIndex { table: table("t"), name: "a".into() });
    draft.push(Change::DropIndex { table: table("t"), name: "b".into() });
    draft.push(Change::DropIndex { table: table("t"), name: "c".into() });
    let applied = [draft.changes[0].clone(), draft.changes[2].clone()];
    assert_eq!(pending(&draft.without_changes(&applied)), vec!["Drop index b on shop.t"]);
}

// ---- applying ------------------------------------------------------------------------------

/// Records every statement; fails on the `fail_on`th statement (counting BEGIN etc.).
struct Recorder {
    ran: Vec<String>,
    fail_containing: Option<&'static str>,
    fail_commit: bool,
}

impl StatementRunner for Recorder {
    fn run(&mut self, sql: &str) -> Result<(), String> {
        if self.fail_containing.is_some_and(|needle| sql.contains(needle)) {
            return Err(format!("boom on {sql}"));
        }
        if self.fail_commit && sql == "COMMIT" {
            return Err("commit refused".into());
        }
        self.ran.push(sql.to_owned());
        Ok(())
    }
}

fn three_drops() -> Vec<Change> {
    ["a", "b", "c"].map(|n| Change::DropIndex { table: table("t"), name: n.into() }).to_vec()
}

#[test]
fn a_transactional_dialect_rolls_everything_back_on_the_first_error() {
    let planned = plan(dialect(ConnectionKind::Postgres), &three_drops());
    let mut runner = Recorder { ran: Vec::new(), fail_containing: Some("\"b\""), fail_commit: false };
    let outcome = apply(&planned, &mut runner);
    // The session is prepared inside the transaction (SET LOCAL ends with it), then the batch runs.
    assert_eq!(
        runner.ran,
        vec![
            "BEGIN",
            "SET LOCAL lock_timeout = '10s'",
            "SET LOCAL statement_timeout = 0",
            "SET LOCAL standard_conforming_strings = on",
            "DROP INDEX \"shop\".\"a\"",
            "ROLLBACK",
        ]
    );
    assert!(outcome.rolled_back && outcome.applied.is_empty());
    assert_eq!(outcome.failed.as_ref().map(|f| f.step), Some(1));
    assert!(outcome.completed_changes(&planned).is_empty());
    let report = outcome.report(&planned.steps());
    assert!(report.contains("Statement 2 of 3 failed"), "{report}");
    assert!(report.contains("rolled back: nothing was applied"), "{report}");
}

#[test]
fn a_transactional_dialect_commits_when_everything_ran() {
    let planned = plan(dialect(ConnectionKind::Sqlite), &three_drops());
    let mut runner = Recorder { ran: Vec::new(), fail_containing: None, fail_commit: false };
    let outcome = apply(&planned, &mut runner);
    // SQLite bounds the lock wait before the transaction starts.
    assert_eq!(runner.ran.first().map(String::as_str), Some("PRAGMA busy_timeout = 10000"));
    assert_eq!(runner.ran.get(1).map(String::as_str), Some("BEGIN"));
    assert_eq!(runner.ran.last().map(String::as_str), Some("COMMIT"));
    assert!(outcome.succeeded());
    assert_eq!(outcome.completed_changes(&planned), vec![0, 1, 2]);
    assert_eq!(outcome.report(&planned.steps()), "Applied 3 statements.");
}

#[test]
fn a_failed_commit_is_a_rollback() {
    let planned = plan(dialect(ConnectionKind::Postgres), &three_drops());
    let mut runner = Recorder { ran: Vec::new(), fail_containing: None, fail_commit: true };
    let outcome = apply(&planned, &mut runner);
    assert!(!outcome.succeeded() && outcome.rolled_back && outcome.applied.is_empty());
    assert!(outcome.failed.unwrap().message.contains("COMMIT failed"));
}

#[test]
fn a_dialect_without_transactions_stops_at_the_error_and_reports_what_ran() {
    let planned = plan(dialect(ConnectionKind::Mysql), &three_drops());
    let mut runner = Recorder { ran: Vec::new(), fail_containing: Some("`c`"), fail_commit: false };
    let outcome = apply(&planned, &mut runner);
    assert_eq!(
        runner.ran,
        vec!["SET SESSION lock_wait_timeout = 10", "DROP INDEX `a` ON `shop`.`t`", "DROP INDEX `b` ON `shop`.`t`"]
    );
    assert_eq!(outcome.applied, vec![0, 1]);
    assert_eq!(outcome.completed_changes(&planned), vec![0, 1], "the changes that ran leave the draft");
    let report = outcome.report(&planned.steps());
    assert!(report.contains("Statement 3 of 3 failed"), "{report}");
    assert!(report.contains("cannot roll DDL back"), "{report}");
    assert!(report.contains("1. DROP INDEX `a`"), "{report}");
    assert!(report.contains("2. DROP INDEX `b`"), "{report}");
}

#[test]
fn nothing_runs_when_a_change_cannot_be_generated() {
    let planned = plan(
        dialect(ConnectionKind::Clickhouse),
        &[Change::AddForeignKey { table: table("t"), foreign_key: fk("f") }],
    );
    let mut runner = Recorder { ran: Vec::new(), fail_containing: None, fail_commit: false };
    let outcome = apply(&planned, &mut runner);
    assert!(runner.ran.is_empty());
    assert!(!outcome.succeeded());
}

// ---- defaults and free text (B3 and the lexer) ----------------------------------------------

#[test]
fn a_catalog_default_with_a_backslash_is_restated_only_where_the_server_reads_it_as_the_catalog_does() {
    // The catalog prints a stored `a\b` as 'a\\b'. With backslash escapes that reads back as one backslash;
    // under NO_BACKSLASH_ESCAPES it would be two, so the column is not restated there.
    let t = table("orders");
    let before = ColumnSpec {
        default: Some(r"'C:\\temp'".into()),
        attributes: Some(attributes("", Some(("utf8mb4", "utf8mb4_bin")))),
        ..column("path", "varchar(40)", true)
    };
    let widened = ColumnSpec { data_type: "varchar(80)".into(), ..before.clone() };
    let change = Change::AlterColumn { table: t, from: before.clone(), to: widened.clone() };
    for kind in [ConnectionKind::Mysql, ConnectionKind::Mariadb] {
        let usual = dialect_in_mode(kind, "STRICT_TRANS_TABLES").unwrap();
        let planned = plan(&*usual, std::slice::from_ref(&change));
        assert!(planned.script().contains(r"DEFAULT 'C:\\temp'"), "{}", planned.script());
        let strict = dialect_in_mode(kind, "NO_BACKSLASH_ESCAPES").unwrap();
        let error = strict.statements(&change, &[]).unwrap_err().to_string();
        assert!(error.contains("backslash") && error.contains("NO_BACKSLASH_ESCAPES"), "{error}");
        // A default the user typed is quoted for the mode (one backslash stays one backslash).
        let typed = ColumnSpec { default: Some(r"C:\new".into()), ..widened.clone() };
        let sql = strict.statements(&Change::AlterColumn { table: table("orders"), from: before.clone(), to: typed }, &[]).unwrap();
        assert!(sql[0].contains(r"DEFAULT 'C:\new'"), "{sql:?}");
        // A column without backslashes is restated in either mode.
        let plain = ColumnSpec { default: Some("'x'".into()), ..before.clone() };
        let wider = ColumnSpec { data_type: "varchar(80)".into(), ..plain.clone() };
        assert!(strict.statements(&Change::AlterColumn { table: table("orders"), from: plain, to: wider }, &[]).is_ok());
    }
}

#[test]
fn the_catalogs_own_defaults_survive_a_type_change_verbatim() {
    use ConnectionKind::Postgres as Pg;
    let t = table("orders");
    for default in ["NULL::character varying", "CURRENT_USER", "nextval('seq'::regclass)", "'a b'::text", "(1 + 2)"] {
        let before = ColumnSpec { default: Some(default.into()), ..column("c", "character varying(10)", true) };
        let after = ColumnSpec { data_type: "text".into(), ..before.clone() };
        assert_eq!(
            sql(Pg, Change::AlterColumn { table: t.clone(), from: before, to: after }),
            vec![format!(
                "ALTER TABLE \"shop\".\"orders\" ALTER COLUMN \"c\" DROP DEFAULT, ALTER COLUMN \"c\" TYPE text USING \"c\"::text, ALTER COLUMN \"c\" SET DEFAULT {default}"
            )],
            "{default}"
        );
    }
    // Without a type change an unchanged default is not touched at all.
    let before = ColumnSpec { default: Some("CURRENT_USER".into()), ..column("c", "text", true) };
    let commented = ColumnSpec { comment: Some("who".into()), ..before.clone() };
    assert_eq!(
        sql(Pg, Change::AlterColumn { table: t.clone(), from: before.clone(), to: commented }),
        vec!["COMMENT ON COLUMN \"shop\".\"orders\".\"c\" IS 'who'"]
    );
    // Only what the user typed is classified: here a changed default becomes a literal.
    let typed = ColumnSpec { default: Some("anonymous".into()), ..before.clone() };
    assert_eq!(
        sql(Pg, Change::AlterColumn { table: t.clone(), from: before, to: typed }),
        vec!["ALTER TABLE \"shop\".\"orders\" ALTER COLUMN \"c\" SET DEFAULT 'anonymous'"]
    );
    // MySQL and ClickHouse restate the unchanged default as printed, too.
    let my_before = ColumnSpec {
        default: Some("CURRENT_USER()".into()),
        attributes: Some(attributes("", Some(("utf8mb4", "utf8mb4_0900_ai_ci")))),
        ..column("c", "varchar(20)", true)
    };
    let my_after = ColumnSpec { data_type: "varchar(40)".into(), ..my_before.clone() };
    assert!(sql(ConnectionKind::Mysql, Change::AlterColumn { table: t.clone(), from: my_before, to: my_after })[0].contains("DEFAULT CURRENT_USER()"));
    let ch_before = ColumnSpec { default: Some("currentUser()".into()), ..column("c", "String", false) };
    let ch_after = ColumnSpec { data_type: "FixedString(64)".into(), ..ch_before.clone() };
    assert!(sql(ConnectionKind::Clickhouse, Change::AlterColumn { table: t, from: ch_before, to: ch_after })[0].ends_with("DEFAULT currentUser()"));
}

#[test]
fn typed_text_stays_one_expression() {
    let pg = dialect(ConnectionKind::Postgres);
    let add = |column: ColumnSpec| pg.statements(&Change::AddColumn { table: table("t"), column }, &[]);
    // A comma outside parentheses would end the column definition: the statement would add a second action.
    assert!(add(column("c", "int, DROP COLUMN important", true)).is_err());
    assert!(add(ColumnSpec { default: Some("(0), DROP COLUMN x".into()), ..column("c", "int", true) }).is_err());
    // Text that is not an expression is quoted whole, however it reads.
    let quoted = add(ColumnSpec { default: Some("0, ADD COLUMN y int".into()), ..column("c", "int", true) }).unwrap();
    assert!(quoted[0].ends_with("DEFAULT '0, ADD COLUMN y int'"), "{quoted:?}");
    assert!(add(ColumnSpec { default: Some("fn(1), ADD COLUMN y int".into()), ..column("c", "int", true) }).is_err());
    // Keywords that start another action, or add an attribute, are not part of a type.
    for bad in ["int DROP COLUMN x", "int ADD COLUMN y int", "int RENAME TO z", "int NOT NULL", "int DEFAULT 1", "int COMMENT 'x'", "int REFERENCES other (id)"] {
        let error = add(column("c", bad, true)).unwrap_err().to_string();
        assert!(error.contains("cannot be part of a type"), "{bad}: {error}");
    }
    // Commas inside parentheses, quoted keywords and legitimate multi-word types are fine.
    for good in ["numeric(10, 2)", "timestamp with time zone", "double precision", "character varying(20)", "enum('drop','add')", "int[]"] {
        assert!(add(column("c", good, true)).is_ok(), "{good}");
    }
    assert!(add(ColumnSpec { default: Some("coalesce(a, b)".into()), ..column("c", "int", true) }).is_ok());
    assert!(add(ColumnSpec { default: Some("'a, b'".into()), ..column("c", "text", true) }).is_ok());
    assert!(add(ColumnSpec { default: Some("now() -- x".into()), ..column("c", "text", true) }).is_err());
    assert!(add(ColumnSpec { default: Some("now() /* x */".into()), ..column("c", "text", true) }).is_err());
}

#[test]
fn a_backslash_changes_what_is_one_literal_under_each_sql_mode() {
    // `'\'; DROP TABLE t; -- '` is a single literal while a backslash escapes the quote, and a literal,
    // a `;` and a comment under NO_BACKSLASH_ESCAPES.
    let typed = r"'\'; DROP TABLE t; -- '";
    let add = |dialect: &dyn DdlDialect| {
        dialect.statements(
            &Change::AddColumn { table: table("t"), column: ColumnSpec { default: Some(typed.into()), ..column("c", "text", true) } },
            &[],
        )
    };
    let default_mode = dialect_in_mode(ConnectionKind::Mysql, "STRICT_TRANS_TABLES,ONLY_FULL_GROUP_BY").unwrap();
    let statements = add(&*default_mode).expect("one literal in the default mode");
    assert_eq!(statements, vec![format!("ALTER TABLE `shop`.`t` ADD COLUMN `c` text NULL DEFAULT {typed}")]);
    let no_backslash = dialect_in_mode(ConnectionKind::Mariadb, "NO_BACKSLASH_ESCAPES,STRICT_TRANS_TABLES").unwrap();
    let error = add(&*no_backslash).unwrap_err().to_string();
    assert!(error.contains("';' is not allowed"), "{error}");
    // Plain text is quoted for the mode: the backslash is doubled only where it is an escape.
    let plain = |dialect: &dyn DdlDialect| {
        dialect
            .statements(&Change::AddColumn { table: table("t"), column: ColumnSpec { default: Some(r"C:\temp".into()), ..column("c", "text", true) } }, &[])
            .unwrap()
            .remove(0)
    };
    assert!(plain(&*default_mode).ends_with(r"DEFAULT 'C:\\temp'"));
    assert!(plain(&*no_backslash).ends_with(r"DEFAULT 'C:\temp'"));
    // Comments are literals too.
    let commented = |dialect: &dyn DdlDialect| {
        dialect
            .statements(&Change::AddColumn { table: table("t"), column: ColumnSpec { comment: Some(r"a\'b".into()), ..column("c", "text", true) } }, &[])
            .unwrap()
            .remove(0)
    };
    assert!(commented(&*default_mode).ends_with(r"COMMENT 'a\\''b'"));
    assert!(commented(&*no_backslash).ends_with(r"COMMENT 'a\''b'"));
    // PostgreSQL does not depend on the mode.
    assert!(!dialect_in_mode(ConnectionKind::Postgres, "NO_BACKSLASH_ESCAPES").unwrap().quoting().backslash_escapes);
}

#[test]
fn typed_text_cannot_hide_a_clause_inside_a_name_the_server_reads_differently() {
    // Under ANSI_QUOTES `"a\"` is the name a\, so the `,` and `DROP COLUMN` after it are code.
    let typed = r#"INT AS ("a\") , DROP COLUMN x -- ")"#;
    let add = |dialect: &dyn DdlDialect| {
        dialect.statements(&Change::AddColumn { table: table("t"), column: column("c", typed, true) }, &[])
    };
    let default_mode = dialect_in_mode(ConnectionKind::Mysql, "STRICT_TRANS_TABLES").unwrap();
    assert!(add(&*default_mode).is_ok(), "one string in the default mode");
    for (kind, mode) in [
        (ConnectionKind::Mysql, "STRICT_TRANS_TABLES,ANSI_QUOTES"),
        (ConnectionKind::Mariadb, "ANSI"),
        (ConnectionKind::Mariadb, "MSSQL"),
    ] {
        let error = add(&*dialect_in_mode(kind, mode).unwrap()).unwrap_err().to_string();
        assert!(error.contains("backslash"), "{mode}: {error}");
    }
    // ClickHouse reads a backslash in a name as an escape (backticks included).
    let hidden = r"INT DEFAULT `a\` , DROP COLUMN x -- `";
    let clickhouse = dialect_for(ConnectionKind::Clickhouse).unwrap();
    let error = clickhouse
        .statements(&Change::AddColumn { table: table("t"), column: column("c", hidden, true) }, &[])
        .unwrap_err()
        .to_string();
    assert!(error.contains("backslash"), "{error}");
    // A name without a backslash is fine under ANSI_QUOTES, and `[…]` holds a `'` under MSSQL.
    let ansi = dialect_in_mode(ConnectionKind::Mariadb, "ANSI_QUOTES").unwrap();
    let ok = |dialect: &dyn DdlDialect, text: &str| {
        dialect.statements(&Change::AddColumn { table: table("t"), column: column("c", text, true) }, &[]).is_ok()
    };
    assert!(ok(&*ansi, r#"INT AS ("a" + 1)"#));
    let mssql = dialect_in_mode(ConnectionKind::Mariadb, "MSSQL").unwrap();
    assert!(ok(&*mssql, "INT AS ([a'b] + 1)"));
    assert!(!ok(&*mssql, "INT AS ([a'b] + 1) , DROP COLUMN x -- '] "));
}

#[test]
fn clickhouse_will_not_edit_the_type_or_default_of_a_generated_column() {
    use ConnectionKind::Clickhouse as Ch;
    let materialized = ColumnSpec { generated: true, default: Some("toDate(ts)".into()), ..column("day", "Date", false) };
    let retyped = ColumnSpec { data_type: "DateTime".into(), ..materialized.clone() };
    let error = refusal(Ch, Change::AlterColumn { table: table("t"), from: materialized.clone(), to: retyped });
    assert!(error.contains("MATERIALIZED"), "{error}");
    let redefaulted = ColumnSpec { default: Some("toDate(other)".into()), ..materialized.clone() };
    assert!(refusal(Ch, Change::AlterColumn { table: table("t"), from: materialized.clone(), to: redefaulted }).contains("MATERIALIZED"));
    // A comment is still fine.
    let commented = ColumnSpec { comment: Some("the day".into()), ..materialized.clone() };
    assert_eq!(
        sql(Ch, Change::AlterColumn { table: table("t"), from: materialized, to: commented }),
        vec!["ALTER TABLE `shop`.`t` COMMENT COLUMN `day` 'the day'"]
    );
}

// ---- draft nits ------------------------------------------------------------------------------

#[test]
fn dropping_a_new_table_that_reuses_a_renamed_tables_old_name_keeps_the_rename() {
    let orders = table("orders");
    let mut draft = Draft::default();
    draft.push(Change::RenameTable { table: orders.clone(), to: "purchases".into() });
    draft.push(Change::CreateTable { table: orders.clone(), columns: vec![column("id", "bigint", false)], primary_key: vec!["id".into()], comment: None });
    draft.push(Change::DropTable { table: orders.clone(), cascade: false });
    assert_eq!(draft.changes, vec![Change::RenameTable { table: orders, to: "purchases".into() }]);
}

#[test]
fn a_primary_key_that_loses_its_last_pending_column_is_dropped_from_the_draft_not_turned_into_a_drop_key() {
    let t = table("orders");
    let mut draft = Draft::default();
    draft.push(Change::AddColumn { table: t.clone(), column: column("code", "text", false) });
    draft.push(Change::SetPrimaryKey {
        table: t.clone(),
        columns: vec!["code".into()],
        previous: Some(PrimaryKeyRef { name: Some("orders_pkey".into()), columns: vec!["id".into()] }),
    });
    draft.push(Change::DropColumn { table: t.clone(), column: "code".into() });
    assert!(draft.is_empty(), "the key stays what the database has: {:?}", draft.changes);
    // A key with columns left only loses that column.
    draft.push(Change::AddColumn { table: t.clone(), column: column("code", "text", false) });
    draft.push(Change::SetPrimaryKey { table: t.clone(), columns: vec!["id".into(), "code".into()], previous: None });
    draft.push(Change::DropColumn { table: t.clone(), column: "code".into() });
    assert!(matches!(draft.changes.as_slice(), [Change::SetPrimaryKey { columns, .. }] if columns == &["id".to_string()]));
}

#[test]
fn a_primary_key_back_to_the_one_the_database_has_leaves_the_draft() {
    let t = table("orders");
    let previous = Some(PrimaryKeyRef { name: Some("orders_pkey".into()), columns: vec!["id".into()] });
    let mut draft = Draft::default();
    draft.push(Change::AddColumn { table: t.clone(), column: column("code", "text", false) });
    draft.push(Change::SetPrimaryKey { table: t.clone(), columns: vec!["id".into(), "code".into()], previous: previous.clone() });
    assert_eq!(draft.len(), 2);
    // Forgetting the added column leaves the key equal to `previous`: nothing to replace.
    draft.push(Change::DropColumn { table: t.clone(), column: "code".into() });
    assert!(draft.is_empty(), "{:?}", draft.changes);
    // Pushed directly, the same key never enters.
    draft.push(Change::SetPrimaryKey { table: t.clone(), columns: vec!["id".into()], previous });
    assert!(draft.is_empty(), "{:?}", draft.changes);
}

// ---- stale drafts and interrupted applies ---------------------------------------------------------

fn live(columns: Vec<ColumnSpec>) -> LiveTable {
    LiveTable { columns, primary_key: None, indexes: vec!["idx".into()], foreign_keys: vec!["fk".into()] }
}

/// A key column renamed in the same change-set as a key change: `previous` follows the rename
/// whichever came first, so the replay (which renames the live key at the rename) finds the key
/// the draft started from.
#[test]
fn a_key_change_with_a_renamed_key_column_is_not_stale_in_either_order() {
    let t = table("orders");
    let key = |columns: &[&str]| PrimaryKeyRef { name: Some("orders_pkey".into()), columns: columns.iter().map(|c| (*c).to_owned()).collect() };
    let mut catalog = live(vec![column("tenant_id", "int", false), column("id", "int", false), column("x", "int", false)]);
    catalog.primary_key = Some(key(&["tenant_id", "id"]));
    let mut schema = LiveSchema::new(false);
    schema.present(t.clone(), catalog);
    let rename = Change::RenameColumn { table: t.clone(), from: "tenant_id".into(), to: "tenant".into() };

    // Rename first: the key is set from the renamed catalog key.
    let mut draft = Draft::default();
    draft.push(rename.clone());
    draft.push(Change::SetPrimaryKey { table: t.clone(), columns: vec!["tenant".into(), "id".into(), "x".into()], previous: Some(key(&["tenant", "id"])) });
    assert!(stale(&draft.changes, &schema).is_empty(), "{:?}", stale(&draft.changes, &schema));

    // Key first, from the catalog's names; the rename that follows renames `previous` too.
    let mut draft = Draft::default();
    draft.push(Change::SetPrimaryKey { table: t.clone(), columns: vec!["tenant_id".into(), "id".into(), "x".into()], previous: Some(key(&["tenant_id", "id"])) });
    draft.push(rename);
    let set = draft.changes.iter().find_map(|c| match c {
        Change::SetPrimaryKey { columns, previous, .. } => Some((columns.clone(), previous.clone())),
        _ => None,
    });
    assert_eq!(set, Some((vec!["tenant".into(), "id".into(), "x".into()], Some(key(&["tenant", "id"])))));
    assert!(stale(&draft.changes, &schema).is_empty(), "{:?}", stale(&draft.changes, &schema));
    // Discarding the rename puts the old names back in both.
    let at = draft.changes.iter().position(|c| matches!(c, Change::RenameColumn { .. })).unwrap();
    assert!(draft.discard(at));
    assert!(stale(&draft.changes, &schema).is_empty(), "{:?}", stale(&draft.changes, &schema));
}

#[test]
fn a_change_whose_table_moved_on_is_stale_and_says_why() {
    let t = table("orders");
    let from = ColumnSpec { default: Some("0".into()), ..column("qty", "integer", true) };
    let changes = vec![
        Change::AlterColumn { table: t.clone(), from: from.clone(), to: ColumnSpec { data_type: "bigint".into(), ..from.clone() } },
        Change::DropColumn { table: t.clone(), column: "gone".into() },
        Change::DropIndex { table: t.clone(), name: "idx".into() },
        Change::AddColumn { table: t.clone(), column: column("fresh", "text", true) },
    ];
    // The catalog matches what the draft started from: nothing is stale (the dropped column is missing: stale).
    let mut schema = LiveSchema::new(false);
    schema.present(t.clone(), live(vec![from.clone()]));
    let found = stale(&changes, &schema);
    assert_eq!(found.iter().map(|(i, _)| *i).collect::<Vec<_>>(), vec![1], "{found:?}");
    // The column was widened outside the app: the alteration is stale.
    let mut schema = LiveSchema::new(false);
    schema.present(t.clone(), live(vec![ColumnSpec { data_type: "bigint".into(), ..from.clone() }, column("gone", "text", true)]));
    let found = stale(&changes, &schema);
    let (index, reason) = &found[0];
    assert_eq!(*index, 0);
    assert!(reason.contains("refresh the diagram") && reason.contains("bigint") && reason.contains("integer"), "{reason}");
    // The table is gone: every change on it is stale; a table nobody described decides nothing.
    let mut schema = LiveSchema::new(false);
    schema.absent(t.clone());
    assert_eq!(stale(&changes, &schema).len(), 4);
    assert!(stale(&changes, &LiveSchema::new(false)).is_empty());
    // A rename moves the names: the live table has the old ones.
    let renamed = vec![
        Change::RenameTable { table: t.clone(), to: "purchases".into() },
        Change::RenameColumn { table: table("purchases"), from: "qty".into(), to: "amount".into() },
        Change::AlterColumn {
            table: table("purchases"),
            from: ColumnSpec { name: "amount".into(), ..from.clone() },
            to: ColumnSpec { name: "amount".into(), data_type: "bigint".into(), ..from.clone() },
        },
    ];
    let mut schema = LiveSchema::new(false);
    schema.present(t.clone(), live(vec![from.clone()]));
    assert!(stale(&renamed, &schema).is_empty(), "{:?}", stale(&renamed, &schema));
    // A table created by the draft must not exist yet; a rename must not land on an existing table.
    let create = vec![Change::CreateTable { table: table("tags"), columns: vec![column("id", "int", false)], primary_key: vec![], comment: None }];
    let mut schema = LiveSchema::new(false);
    schema.present(table("tags"), live(vec![]));
    assert_eq!(stale(&create, &schema).len(), 1);
    let rename = vec![Change::RenameTable { table: t.clone(), to: "tags".into() }];
    let mut schema = LiveSchema::new(false);
    schema.present(t, live(vec![]));
    schema.present(table("tags"), live(vec![]));
    assert_eq!(stale(&rename, &schema).len(), 1);
}

#[test]
fn staleness_is_judged_against_the_catalog_as_the_change_set_evolves_it() {
    let t = table("orders");
    let flagged = |changes: &[Change], schema: &LiveSchema| stale(changes, schema).iter().map(|(i, _)| *i).collect::<Vec<_>>();
    let mut schema = LiveSchema::new(false);
    schema.present(t.clone(), live(vec![column("id", "integer", true)]));
    schema.present(table("tags"), live(vec![]));
    // A name an earlier rename frees can be taken by a created table; so can one an earlier drop frees.
    let reuse = vec![
        Change::RenameTable { table: t.clone(), to: "purchases".into() },
        Change::CreateTable { table: t.clone(), columns: vec![column("id", "int", false)], primary_key: vec![], comment: None },
        Change::DropTable { table: table("tags"), cascade: false },
        Change::CreateTable { table: table("tags"), columns: vec![column("id", "int", false)], primary_key: vec![], comment: None },
    ];
    assert!(flagged(&reuse, &schema).is_empty(), "{:?}", stale(&reuse, &schema));
    // The same create without the rename is still refused: the name is taken.
    assert_eq!(flagged(&reuse[1..2], &schema), vec![0]);
    // A column name an earlier rename or drop frees.
    let columns = vec![
        Change::RenameColumn { table: t.clone(), from: "id".into(), to: "legacy_id".into() },
        Change::AddColumn { table: t.clone(), column: column("id", "bigint", false) },
    ];
    assert!(flagged(&columns, &schema).is_empty(), "{:?}", stale(&columns, &schema));
    // Dropping an index or foreign key and creating one with the same name again is one coherent change-set.
    let recreated = vec![
        Change::DropIndex { table: t.clone(), name: "idx".into() },
        Change::CreateIndex { table: t.clone(), index: IndexSpec { name: "idx".into(), columns: vec!["id".into()], unique: true } },
        Change::DropForeignKey { table: t.clone(), name: "fk".into() },
        Change::AddForeignKey {
            table: t.clone(),
            foreign_key: ForeignKeySpec {
                name: "fk".into(),
                columns: vec!["id".into()],
                ref_table: table("tags"),
                ref_columns: vec!["id".into()],
                on_delete: None,
                on_update: None,
            },
        },
    ];
    assert!(flagged(&recreated, &schema).is_empty(), "{:?}", stale(&recreated, &schema));
    // Creating one that exists, without dropping it, is stale.
    assert_eq!(flagged(&recreated[1..2], &schema), vec![0]);
    // The draft of a column made while a pending primary key already showed it NOT NULL.
    let keyed = vec![
        Change::SetPrimaryKey { table: t.clone(), columns: vec!["id".into()], previous: None },
        Change::AlterColumn { table: t.clone(), from: column("id", "integer", false), to: column("id", "bigint", false) },
    ];
    assert!(flagged(&keyed, &schema).is_empty(), "{:?}", stale(&keyed, &schema));
    // Without the key, the same `from` does not match a nullable column.
    assert_eq!(flagged(&keyed[1..], &schema), vec![0]);
    // A column that really changed outside is still stale next to a pending key.
    let mut changed = LiveSchema::new(false);
    changed.present(t.clone(), live(vec![column("id", "text", true)]));
    assert_eq!(flagged(&keyed, &changed), vec![1]);
}

#[test]
fn the_changes_of_an_interrupted_apply_that_are_in_the_catalog_are_recognised() {
    let t = table("orders");
    let from = column("qty", "integer", true);
    let to = column("qty", "bigint", true);
    let changes = vec![
        Change::AlterColumn { table: t.clone(), from: from.clone(), to: to.clone() },
        Change::AddColumn { table: t.clone(), column: column("fresh", "text", true) },
        Change::DropIndex { table: t.clone(), name: "idx".into() },
        Change::CreateTable { table: table("tags"), columns: vec![column("id", "int", false)], primary_key: vec![], comment: None },
        Change::DropTable { table: table("old"), cascade: false },
    ];
    let mut schema = LiveSchema::new(false);
    // The alteration and the new table ran; the column and the index drop did not; `old` is gone.
    schema.present(t.clone(), live(vec![to]));
    schema.present(table("tags"), live(vec![column("id", "int", false)]));
    schema.absent(table("old"));
    assert_eq!(already_applied(&changes, &schema), vec![0, 3, 4]);
    // Nothing is claimed about a table the catalog knows nothing about.
    assert!(already_applied(&changes, &LiveSchema::new(false)).is_empty());
}

// ---- apply: nothing to do, cancel ---------------------------------------------------------------------

#[test]
fn a_change_with_nothing_to_run_leaves_the_draft_with_a_run_that_succeeded() {
    use ConnectionKind::Mysql as My;
    let same = ColumnSpec { attributes: Some(attributes("", None)), ..column("qty", "int", true) };
    let changes = vec![
        Change::AlterColumn { table: table("t"), from: same.clone(), to: same },
        Change::DropIndex { table: table("t"), name: "a".into() },
    ];
    let planned = plan(dialect(My), &changes);
    assert_eq!(planned.steps().len(), 1);
    let mut runner = Recorder { ran: Vec::new(), fail_containing: None, fail_commit: false };
    let outcome = apply(&planned, &mut runner);
    assert_eq!(outcome.completed_changes(&planned), vec![0, 1]);
    // A failed run does not claim it.
    let mut runner = Recorder { ran: Vec::new(), fail_containing: Some("DROP INDEX"), fail_commit: false };
    let outcome = apply(&planned, &mut runner);
    assert!(outcome.completed_changes(&planned).is_empty());
}

struct Cancelling {
    ran: Vec<String>,
    cancel_after: usize,
    rollback_fails: bool,
}

impl StatementRunner for Cancelling {
    fn run(&mut self, sql: &str) -> Result<(), String> {
        if self.ran.len() >= self.cancel_after && !sql.starts_with("SET") {
            return Err("cancelled by the user".into());
        }
        self.ran.push(sql.to_owned());
        Ok(())
    }
    fn finish(&mut self, sql: &str) -> Result<(), String> {
        self.ran.push(sql.to_owned());
        if self.rollback_fails && sql == "ROLLBACK" {
            return Err("connection lost".into());
        }
        Ok(())
    }
    fn cancelled(&self) -> bool {
        self.ran.len() >= self.cancel_after
    }
}

#[test]
fn cancel_rolls_back_where_it_can_and_reports_what_ran_where_it_cannot() {
    let planned = plan(dialect(ConnectionKind::Postgres), &three_drops());
    // BEGIN and the two SET LOCAL ran, then the user cancelled before the first statement.
    let mut runner = Cancelling { ran: Vec::new(), cancel_after: 3, rollback_fails: false };
    let outcome = apply(&planned, &mut runner);
    assert!(outcome.cancelled && outcome.rolled_back && outcome.applied.is_empty());
    assert_eq!(runner.ran.last().map(String::as_str), Some("ROLLBACK"), "{:?}", runner.ran);
    let report = outcome.report(&planned.steps());
    assert!(report.starts_with("Cancelled.") && report.contains("rolled back"), "{report}");
    // MySQL cannot roll back: after one statement the report names it.
    let planned = plan(dialect(ConnectionKind::Mysql), &three_drops());
    let mut runner = Cancelling { ran: Vec::new(), cancel_after: 2, rollback_fails: false };
    let outcome = apply(&planned, &mut runner);
    assert!(outcome.cancelled && outcome.applied == vec![0]);
    let report = outcome.report(&planned.steps());
    assert!(report.starts_with("Cancelled.") && report.contains("stay applied") && report.contains("1. DROP INDEX `a`"), "{report}");
    assert_eq!(outcome.completed_changes(&planned), vec![0]);
}

#[test]
fn a_cancelled_apply_whose_rollback_failed_does_not_claim_a_rollback() {
    let planned = plan(dialect(ConnectionKind::Postgres), &three_drops());
    let mut runner = Cancelling { ran: Vec::new(), cancel_after: 3, rollback_fails: true };
    let outcome = apply(&planned, &mut runner);
    assert!(outcome.cancelled && !outcome.rolled_back && outcome.rollback_error.as_deref() == Some("connection lost"));
    let report = outcome.report(&planned.steps());
    assert!(report.starts_with("Cancelled.") && report.contains("rollback failed") && report.contains("connection lost"), "{report}");
    assert!(!report.contains("was rolled back"), "{report}");
}

#[test]
fn the_session_is_set_up_so_an_apply_neither_waits_for_ever_nor_is_cut_short() {
    let pg = plan_with_lock_timeout(dialect(ConnectionKind::Postgres), &three_drops(), 3);
    assert_eq!(pg.setup.in_transaction, vec!["SET LOCAL lock_timeout = '3s'", "SET LOCAL statement_timeout = 0", "SET LOCAL standard_conforming_strings = on"]);
    assert!(pg.setup.before.is_empty());
    let my = plan_with_lock_timeout(dialect(ConnectionKind::Mariadb), &three_drops(), 3);
    assert_eq!(my.setup.before, vec!["SET SESSION lock_wait_timeout = 3"]);
    let ch = plan(dialect(ConnectionKind::Clickhouse), &three_drops());
    assert!(ch.setup.before.is_empty() && ch.setup.in_transaction.is_empty(), "ClickHouse has no lock wait to bound");
}

#[test]
fn create_database_is_planned_alone_and_refused_next_to_any_other_change() {
    let create = Change::CreateNamespace { kind: NamespaceKind::Database, name: "reports".into() };
    for kind in [ConnectionKind::Postgres, ConnectionKind::Mysql, ConnectionKind::Mariadb, ConnectionKind::Clickhouse] {
        // Alone it runs, outside a transaction (PostgreSQL cannot create a database inside one).
        let alone = plan(dialect(kind), std::slice::from_ref(&create));
        assert!(alone.executable() && !alone.transactional, "{kind:?}");
        assert_eq!(alone.steps().len(), 1);
        // Next to any other change - or another CREATE DATABASE - the CREATE DATABASE is refused,
        // so the rest never loses its transaction and its session setup.
        for other in [
            Change::DropIndex { table: table("t"), name: "a".into() },
            Change::CreateNamespace { kind: NamespaceKind::Database, name: "second".into() },
        ] {
            let mixed = plan(dialect(kind), &[other, create.clone()]);
            assert!(!mixed.executable(), "{kind:?}");
            let refused = &mixed.entry(1).expect("the CREATE DATABASE has an entry").statements;
            let error = refused.as_ref().expect_err("the CREATE DATABASE is refused");
            assert_eq!(error, &DdlError::CreateDatabaseNotAlone, "{kind:?}");
            assert!(error.to_string().contains("on its own"), "{error}");
        }
    }
    // The refusal is a property of the set: a change-set without CREATE DATABASE keeps its
    // transaction and its setup.
    let pg = plan(dialect(ConnectionKind::Postgres), &three_drops());
    assert!(pg.transactional && pg.executable());
    assert!(!pg.setup.in_transaction.is_empty());
}

// ---- duplicate names are refused by the plan -------------------------------------------------------

fn invalid_at(planned: &Planned, change: usize) -> Option<String> {
    match &planned.entry(change)?.statements {
        Err(DdlError::Invalid(message)) => Some(message.clone()),
        _ => None,
    }
}

fn catalog_table() -> LiveSchema {
    let mut schema = LiveSchema::new(false);
    schema.present(
        table("orders"),
        LiveTable {
            columns: vec![column("id", "bigint", false), column("note", "text", true)],
            primary_key: None,
            indexes: vec!["orders_note_idx".into()],
            foreign_keys: vec!["orders_customer_fk".into()],
        },
    );
    schema
}

#[test]
fn a_change_set_that_repeats_a_column_index_or_foreign_key_name_is_refused_without_a_catalog() {
    let t = table("orders");
    let changes = vec![
        Change::RenameColumn { table: t.clone(), from: "a".into(), to: "x".into() },
        Change::AddColumn { table: t.clone(), column: column("x", "text", true) },
        Change::CreateIndex { table: t.clone(), index: index("idx", false) },
        Change::CreateIndex { table: t.clone(), index: index("idx", true) },
        Change::AddForeignKey { table: t.clone(), foreign_key: fk("fk") },
        Change::AddForeignKey { table: t.clone(), foreign_key: fk("fk") },
    ];
    for kind in [ConnectionKind::Postgres, ConnectionKind::Mysql, ConnectionKind::Mariadb] {
        let planned = plan(dialect(kind), &changes);
        assert!(!planned.executable(), "{kind:?}");
        assert!(planned.entry(0).unwrap().statements.is_ok(), "the rename itself is fine");
        assert_eq!(invalid_at(&planned, 1).as_deref(), Some("A column named \"x\" already exists in shop.orders."), "{kind:?}");
        assert!(planned.entry(2).unwrap().statements.is_ok());
        assert_eq!(invalid_at(&planned, 3).as_deref(), Some("An index named \"idx\" already exists on shop.orders."), "{kind:?}");
        assert!(planned.entry(4).unwrap().statements.is_ok());
        assert_eq!(invalid_at(&planned, 5).as_deref(), Some("A foreign key named \"fk\" already exists on shop.orders."), "{kind:?}");
    }
    // Two columns renamed onto one name.
    let planned = plan(
        dialect(ConnectionKind::Postgres),
        &[
            Change::RenameColumn { table: t.clone(), from: "a".into(), to: "x".into() },
            Change::RenameColumn { table: t, from: "b".into(), to: "x".into() },
        ],
    );
    assert_eq!(invalid_at(&planned, 1).as_deref(), Some("A column named \"x\" already exists in shop.orders."));
}

#[test]
fn a_name_another_change_frees_can_be_taken_again() {
    let t = table("orders");
    for kind in [ConnectionKind::Postgres, ConnectionKind::Mysql] {
        let planned = plan(
            dialect(kind),
            &[
                Change::AddColumn { table: t.clone(), column: column("a", "text", true) },
                Change::RenameColumn { table: t.clone(), from: "a".into(), to: "b".into() },
                Change::DropColumn { table: t.clone(), column: "c".into() },
                Change::AddColumn { table: t.clone(), column: column("c", "text", true) },
                Change::DropIndex { table: t.clone(), name: "idx".into() },
                Change::CreateIndex { table: t.clone(), index: index("idx", false) },
                Change::DropForeignKey { table: t.clone(), name: "fk".into() },
                Change::AddForeignKey { table: t.clone(), foreign_key: fk("fk") },
            ],
        );
        assert!(planned.executable(), "{kind:?}: {:?}", planned.first_error());
    }
}

#[test]
fn the_catalog_makes_a_taken_name_a_refusal_of_the_plan() {
    let t = table("orders");
    let changes = vec![
        Change::AddColumn { table: t.clone(), column: column("note", "text", true) },
        Change::RenameColumn { table: t.clone(), from: "id".into(), to: "note".into() },
        Change::CreateIndex { table: t.clone(), index: index("orders_note_idx", false) },
        Change::AddForeignKey { table: t.clone(), foreign_key: fk("orders_customer_fk") },
        Change::AddColumn { table: t.clone(), column: column("fresh", "text", true) },
        Change::CreateIndex { table: t.clone(), index: index("fresh_idx", false) },
    ];
    // Without the catalog the index and the foreign key names look free; with it they are not.
    let blind = plan(dialect(ConnectionKind::Postgres), &changes);
    assert!(blind.entry(2).unwrap().statements.is_ok() && blind.entry(3).unwrap().statements.is_ok());
    let planned = plan_against(dialect(ConnectionKind::Postgres), &changes, &catalog_table());
    assert_eq!(invalid_at(&planned, 0).as_deref(), Some("A column named \"note\" already exists in shop.orders."));
    assert_eq!(invalid_at(&planned, 1).as_deref(), Some("A column named \"note\" already exists in shop.orders."));
    assert_eq!(invalid_at(&planned, 2).as_deref(), Some("An index named \"orders_note_idx\" already exists on shop.orders."));
    assert_eq!(invalid_at(&planned, 3).as_deref(), Some("A foreign key named \"orders_customer_fk\" already exists on shop.orders."));
    assert!(planned.entry(4).unwrap().statements.is_ok() && planned.entry(5).unwrap().statements.is_ok());
    assert!(!planned.executable());
    // A refused change contributes no SQL to the script.
    assert!(!planned.script().contains("orders_note_idx"));
    assert!(planned.script().contains("fresh_idx"));
    // Dropping the old one first makes the name free.
    let mut with_drop = changes.clone();
    with_drop.push(Change::DropIndex { table: t, name: "orders_note_idx".into() });
    let planned = plan_against(dialect(ConnectionKind::Postgres), &with_drop, &catalog_table());
    assert!(invalid_at(&planned, 2).is_none());
}

// ---- name clashes: shapes that must not be reported ------------------------------------------------

fn clashes(changes: &[Change], live: &LiveSchema) -> Vec<(usize, String)> {
    schema::name_clashes(changes, live, NameRules::EXACT)
}

/// The catalog of `orders` as the live scenario creates it (before the table is renamed).
fn orders_catalog() -> LiveSchema {
    let mut schema = LiveSchema::new(false);
    schema.present(
        table("orders"),
        LiveTable {
            columns: vec![column("id", "bigint", false), column("customer_id", "bigint", true), column("note", "text", true), column("spare", "text", true)],
            primary_key: Some(PrimaryKeyRef { name: Some("orders_pkey".into()), columns: vec!["id".into()] }),
            indexes: vec!["idx_note".into()],
            foreign_keys: vec!["fk_customer".into()],
        },
    );
    schema
}

/// The change-set of the live scenario: a table renamed, a column added, dropped, altered and
/// renamed, an index and a foreign key swapped, the key replaced; all through the draft, so every
/// later change names the table `purchases` and the altered column `memo`.
fn renamed_table_draft() -> Draft {
    let t = table("orders");
    let mut draft = Draft::default();
    draft.push(Change::AddColumn { table: t.clone(), column: column("status", "varchar(20)", false) });
    draft.push(Change::DropColumn { table: t.clone(), column: "spare".into() });
    draft.push(Change::AlterColumn { table: t.clone(), from: column("note", "text", true), to: column("note", "text", false) });
    draft.push(Change::RenameColumn { table: t.clone(), from: "note".into(), to: "memo".into() });
    draft.push(Change::DropIndex { table: t.clone(), name: "idx_note".into() });
    draft.push(Change::CreateIndex { table: t.clone(), index: IndexSpec { name: "idx_memo".into(), columns: vec!["memo".into()], unique: true } });
    draft.push(Change::DropForeignKey { table: t.clone(), name: "fk_customer".into() });
    draft.push(Change::AddForeignKey { table: t.clone(), foreign_key: ForeignKeySpec { name: "fk2".into(), ..fk("fk2") } });
    draft.push(Change::SetPrimaryKey {
        table: t.clone(),
        columns: vec!["id".into(), "status".into()],
        previous: Some(PrimaryKeyRef { name: Some("orders_pkey".into()), columns: vec!["id".into()] }),
    });
    draft.push(Change::RenameTable { table: t, to: "purchases".into() });
    draft
}

#[test]
fn a_table_rename_with_an_alteration_and_a_rename_of_the_same_column_clashes_with_nothing() {
    let draft = renamed_table_draft();
    // The draft rewrote the alteration to the name the column has after the rename.
    assert!(draft.changes.iter().any(|c| matches!(c, Change::AlterColumn { from, to, .. } if from.name == "memo" && to.name == "memo")), "{:?}", draft.changes);
    for kind in [ConnectionKind::Postgres, ConnectionKind::Mysql, ConnectionKind::Mariadb] {
        // Without a catalog, and with the complete catalog of the table (keyed by its old name).
        assert_eq!(clashes(&draft.changes, &LiveSchema::new(false)), Vec::<(usize, String)>::new(), "{kind:?} without a catalog");
        assert_eq!(clashes(&draft.changes, &orders_catalog()), Vec::<(usize, String)>::new(), "{kind:?} with the catalog");
        // (MySQL restates a column from its reported attributes, which this data has none of:
        // that refusal is `Unsupported`, never a name clash.)
        for planned in [plan(dialect(kind), &draft.changes), plan_against(dialect(kind), &draft.changes, &orders_catalog())] {
            assert!(!planned.entries.iter().any(|entry| matches!(entry.statements, Err(DdlError::Invalid(_)))), "{kind:?}: {:?}", planned.first_error());
        }
    }
}

#[test]
fn renames_that_swap_or_chain_names_are_judged_as_the_server_runs_them() {
    let t = table("orders");
    let rename = |from: &str, to: &str| Change::RenameColumn { table: t.clone(), from: from.into(), to: to.into() };
    // A -> B then B -> A: the draft folds it away.
    let mut draft = Draft::default();
    draft.push(rename("a", "b"));
    draft.push(rename("b", "a"));
    assert!(draft.is_empty(), "{:?}", draft.changes);
    // X -> Y while Y -> Z: in an order that frees Y first the names never meet ...
    assert!(clashes(&[rename("y", "z"), rename("x", "y")], &LiveSchema::new(false)).is_empty());
    // ... and run the other way round the server itself would refuse the first rename.
    let backwards = clashes(&[rename("x", "y"), rename("y", "z")], &LiveSchema::new(false));
    assert_eq!(backwards.iter().map(|(at, _)| *at).collect::<Vec<_>>(), [0]);
    // Hand-built swap: both names exist, the first rename cannot run.
    assert_eq!(clashes(&[rename("a", "b"), rename("b", "a")], &LiveSchema::new(false)).len(), 1);
}

#[test]
fn a_dropped_column_frees_its_name_for_a_new_one_and_a_kept_one_does_not() {
    let t = table("orders");
    let drop_then_add = [
        Change::DropColumn { table: t.clone(), column: "note".into() },
        Change::AddColumn { table: t.clone(), column: column("note", "int", true) },
    ];
    assert!(clashes(&drop_then_add, &LiveSchema::new(false)).is_empty());
    assert!(clashes(&drop_then_add, &orders_catalog()).is_empty(), "with the catalog, the drop comes first");
    let add_only = [drop_then_add[1].clone()];
    assert_eq!(clashes(&add_only, &orders_catalog()).len(), 1, "a column that is kept still has its name");
    // Altered and renamed: the column leaves the name it was altered under.
    let mut draft = Draft::default();
    draft.push(Change::AlterColumn { table: t.clone(), from: column("note", "text", true), to: column("note", "text", false) });
    draft.push(Change::RenameColumn { table: t.clone(), from: "note".into(), to: "memo".into() });
    draft.push(Change::AddColumn { table: t, column: column("note", "text", true) });
    assert!(clashes(&draft.changes, &LiveSchema::new(false)).is_empty(), "{:?}", draft.changes);
    assert!(clashes(&draft.changes, &orders_catalog()).is_empty());
}

#[test]
fn names_are_compared_the_way_each_server_compares_them() {
    let t = table("orders");
    let add = |name: &str| Change::AddColumn { table: t.clone(), column: column(name, "text", true) };
    let index_named = |name: &str| Change::CreateIndex { table: t.clone(), index: index(name, false) };
    let key_named = |name: &str| Change::AddForeignKey { table: t.clone(), foreign_key: fk(name) };
    let mut live = orders_catalog();
    live.present(
        t.clone(),
        LiveTable {
            columns: vec![column("note", "text", true)],
            primary_key: None,
            indexes: vec!["idx_note".into()],
            foreign_keys: vec!["fk_customer".into()],
        },
    );
    // PostgreSQL and ClickHouse quote every name: a case-only difference is another name.
    for kind in [ConnectionKind::Postgres, ConnectionKind::Clickhouse] {
        let planned = plan_against(dialect(kind), &[add("Note"), index_named("IDX_NOTE"), key_named("FK_CUSTOMER")], &live);
        assert!(!planned.entries.iter().any(|entry| matches!(entry.statements, Err(DdlError::Invalid(_)))), "{kind:?}");
    }
    // MySQL, MariaDB (checked against live servers: a foreign key `fk1` next to `Fk1` and an index `ix`
    // next to `Ix` are refused) and SQLite compare without case.
    for kind in [ConnectionKind::Mysql, ConnectionKind::Mariadb, ConnectionKind::Sqlite] {
        let planned = plan_against(dialect(kind), &[add("Note"), index_named("IDX_NOTE"), key_named("FK_CUSTOMER")], &live);
        let refused: Vec<usize> = planned.entries.iter().filter(|e| matches!(e.statements, Err(DdlError::Invalid(_)))).map(|e| e.change).collect();
        assert_eq!(refused, [0, 1, 2], "{kind:?}");
        // The name the column already has is no clash with itself: only its case changes.
        let recase = [Change::RenameColumn { table: t.clone(), from: "note".into(), to: "Note".into() }];
        assert!(clashes_in(kind, &recase, &live).is_empty(), "{kind:?}");
    }
    // Pending names are compared the same way.
    let pending = [add("memo"), add("MEMO")];
    assert_eq!(clashes_in(ConnectionKind::Mysql, &pending, &LiveSchema::new(false)).len(), 1);
    assert!(clashes_in(ConnectionKind::Postgres, &pending, &LiveSchema::new(false)).is_empty());
}

fn clashes_in(kind: ConnectionKind, changes: &[Change], live: &LiveSchema) -> Vec<(usize, String)> {
    schema::name_clashes(changes, live, dialect(kind).name_rules())
}

#[test]
fn a_rename_whose_source_is_gone_says_so_instead_of_naming_a_clash() {
    let t = table("orders");
    // `gone` is not in the catalog and `note` (the target) is: the reason is the missing source.
    let changes = [Change::RenameColumn { table: t.clone(), from: "gone".into(), to: "note".into() }];
    let mut live = LiveSchema::new(false);
    live.present(t, LiveTable { columns: vec![column("note", "text", true)], ..LiveTable::default() });
    assert!(clashes(&changes, &live).is_empty());
    let stale = stale(&changes, &live);
    assert!(stale.len() == 1 && stale[0].1.contains("column gone no longer exists"), "{stale:?}");
}
