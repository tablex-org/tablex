//! Ready-made fixtures for PostgreSQL, MySQL/MariaDB, SQLite and ClickHouse.

use crate::fixture::{MutationTarget, Scratch, SqlFixture};
use engine_driver_api::sql::SqlObjectRef;
use serde_json::{json, Value};

/// PostgreSQL: the scratch is a schema in the connection's database.
#[derive(Debug, Default, Clone, Copy)]
pub struct PostgresFixture;

impl SqlFixture for PostgresFixture {
    fn label(&self) -> &str {
        "postgresql"
    }
    fn driver_id(&self) -> &str {
        "postgresql"
    }
    fn identifier_quote(&self) -> &str {
        "\""
    }
    fn supports_schemas(&self) -> bool {
        true
    }
    fn current_database_query(&self) -> String {
        "SELECT current_database()".into()
    }
    fn create_scratch(&self, scratch: &Scratch) -> Vec<String> {
        vec![format!("CREATE SCHEMA {}", scratch.name)]
    }
    fn drop_scratch(&self, scratch: &Scratch) -> Vec<String> {
        vec![format!("DROP SCHEMA IF EXISTS {} CASCADE", scratch.name)]
    }
    fn create_tables(&self, scratch: &Scratch) -> Vec<String> {
        let s = &scratch.name;
        vec![
            format!("CREATE TABLE {s}.items (tenant INT NOT NULL, id INT NOT NULL, name VARCHAR(100) NULL, qty INT NULL, PRIMARY KEY (tenant, id))"),
            format!("CREATE TABLE {s}.refs (id INT PRIMARY KEY, tenant INT NOT NULL, item_id INT NOT NULL, FOREIGN KEY (tenant, item_id) REFERENCES {s}.items (tenant, id))"),
            format!("CREATE TABLE {s}.notes (owner VARCHAR(20) NULL, body VARCHAR(50) NULL)"),
            format!("CREATE TABLE {s}.dups (k INT, v INT)"),
        ]
    }
    fn table(&self, scratch: &Scratch, table: &str) -> String {
        format!("{}.{table}", scratch.name)
    }
    fn numbers(&self, n: u64) -> String {
        format!("SELECT g AS n FROM generate_series(1, {n}) AS g ORDER BY g")
    }
    fn slow_query(&self, seconds: u32) -> String {
        format!("SELECT pg_sleep({seconds})")
    }
    fn backend_id_query(&self) -> String {
        "SELECT pg_backend_pid()".into()
    }
    fn interrupt_backend_statement(&self, id: &str) -> String {
        format!("SELECT pg_cancel_backend({id})")
    }
    fn write_statements(&self, scratch: &Scratch) -> Vec<String> {
        let t = self.table(scratch, "items");
        vec![
            format!("INSERT INTO {t} (tenant, id, name, qty) VALUES (9, 1, 'w1', 1)"),
            format!("INSERT INTO {t} (tenant, id, name, qty) VALUES (9, 2, 'w2', 2)"),
            format!("INSERT INTO {t} (tenant, id, name, qty) VALUES (9, 3, 'w3', 3)"),
        ]
    }
    fn write_returning_statements(&self, scratch: &Scratch) -> Vec<String> {
        let t = self.table(scratch, "items");
        vec![
            format!("INSERT INTO {t} (tenant, id, name, qty) VALUES (8, 1, 'r1', 1) RETURNING tenant, id"),
            format!("WITH ins AS (INSERT INTO {t} (tenant, id, name, qty) VALUES (8, 2, 'r2', 2) RETURNING tenant, id) SELECT * FROM ins"),
        ]
    }
    fn grant_statement(&self, scratch: &Scratch) -> String {
        format!("GRANT SELECT ON {} TO PUBLIC", self.table(scratch, "items"))
    }
    fn returning_write(&self, scratch: &Scratch, tenant: u32) -> Option<String> {
        let t = self.table(scratch, "items");
        Some(format!(
            "INSERT INTO {t} (tenant, id, name, qty) VALUES ({tenant}, 1, 'a', 1), ({tenant}, 2, 'b', 2), ({tenant}, 3, 'c', 3) RETURNING tenant, id"
        ))
    }
    fn nested_with_delete_statement(&self, scratch: &Scratch) -> Option<String> {
        let t = self.table(scratch, "items");
        Some(format!(
            "WITH x AS (WITH y AS (SELECT 1 AS one) DELETE FROM {t} RETURNING tenant) SELECT * FROM x"
        ))
    }
    fn nested_with_variants(&self, scratch: &Scratch) -> Vec<(String, bool)> {
        let t = self.table(scratch, "items");
        let recursive =
            "WITH RECURSIVE y(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM y WHERE n < 3)";
        let insert = |tenant: u32| {
            format!("INSERT INTO {t} (tenant, id, name, qty) VALUES ({tenant}, 1, 'n', 1) RETURNING tenant")
        };
        vec![
            (format!("WITH x AS ({recursive} SEARCH DEPTH FIRST BY n SET ord DELETE FROM {t} RETURNING tenant) SELECT * FROM x"), true),
            (format!("WITH x AS ({recursive} CYCLE n SET is_cycle USING path DELETE FROM {t} RETURNING tenant) SELECT * FROM x"), true),
            (format!("WITH x AS ({recursive} SEARCH DEPTH FIRST BY n SET ord {}) SELECT * FROM x", insert(50)), false),
            (format!("WITH x AS ({recursive} CYCLE n SET is_cycle USING path {}) SELECT * FROM x", insert(51)), false),
        ]
    }
    fn bulk_object_statements(&self, scratch: &Scratch, count: u32) -> Option<Vec<String>> {
        let s = &scratch.name;
        // Two batches keep each transaction under the lock-table limit.
        let half = count / 2;
        Some(vec![
            format!("DO $$ BEGIN FOR i IN 1..{half} LOOP EXECUTE format('CREATE TABLE {s}.b%s (c int)', i); END LOOP; END $$"),
            format!("DO $$ BEGIN FOR i IN {}..{count} LOOP EXECUTE format('CREATE TABLE {s}.b%s (c int)', i); END LOOP; END $$", half + 1),
        ])
    }
    fn bulk_object_cleanup(&self, scratch: &Scratch, count: u32) -> Vec<String> {
        let s = &scratch.name;
        (1..=count)
            .step_by(1000)
            .map(|from| {
                let to = (from + 999).min(count);
                format!("DO $$ BEGIN FOR i IN {from}..{to} LOOP EXECUTE format('DROP TABLE IF EXISTS {s}.b%s', i); END LOOP; END $$")
            })
            .collect()
    }
    fn slow_mutation_setup(&self, scratch: &Scratch, _uri: &str) -> Option<Vec<String>> {
        let s = &scratch.name;
        Some(vec![
            format!("CREATE TABLE {s}.slow (id INT)"),
            format!("CREATE FUNCTION {s}.slow_trigger() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(20); RETURN NEW; END $$"),
            format!("CREATE TRIGGER slow_insert BEFORE INSERT ON {s}.slow FOR EACH ROW EXECUTE FUNCTION {s}.slow_trigger()"),
        ])
    }
    fn zero_row_insert_setup(&self, scratch: &Scratch) -> Option<Vec<String>> {
        let s = &scratch.name;
        Some(vec![
            format!("CREATE TABLE {s}.swallow (id INT)"),
            format!("CREATE RULE swallow_ignore AS ON INSERT TO {s}.swallow DO INSTEAD NOTHING"),
        ])
    }
    fn navigator_tables_request(&self, scratch: &Scratch) -> Value {
        json!({
            "parent": null,
            "database": scratch.database,
            "payload": {"database": scratch.database, "schema": scratch.name},
            "cursor": null,
            "limit": 0
        })
    }
    fn object_ref(&self, scratch: &Scratch, table: &str) -> SqlObjectRef {
        SqlObjectRef {
            database: Some(scratch.database.clone()),
            schema: Some(scratch.name.clone()),
            object: table.into(),
        }
    }
    fn mutation_target(&self, scratch: &Scratch, table: &str) -> MutationTarget {
        MutationTarget {
            database: None,
            object: table.into(),
            identity: json!({"schema": scratch.name}),
        }
    }
    fn drop_database(&self, name: &str) -> Vec<String> {
        vec![format!("DROP DATABASE IF EXISTS {name}")]
    }
}

/// MySQL and MariaDB: the scratch is a database.
#[derive(Debug, Clone, Copy)]
pub struct MysqlFixture {
    /// MariaDB instead of MySQL (own driver id, statement time limit syntax,
    /// `INSERT ... RETURNING`).
    pub mariadb: bool,
}

impl MysqlFixture {
    pub const MYSQL: Self = Self { mariadb: false };
    pub const MARIADB: Self = Self { mariadb: true };

    /// The user of a `scheme://user:password@host/...` URI (`tablex` if absent).
    fn uri_user(uri: &str) -> &str {
        uri.split_once("://")
            .and_then(|(_, rest)| rest.rsplit_once('@'))
            .map_or("tablex", |(userinfo, _)| {
                userinfo.split_once(':').map_or(userinfo, |(user, _)| user)
            })
    }

    /// A derived table of 1 000 000 rows `n` = 1..=1000000 (digit-table cross
    /// join: works with any recursion limit); the caller adds the alias.
    fn million() -> &'static str {
        "(SELECT a.d + 10 * b.d + 100 * c.d + 1000 * d.d + 10000 * e.d + 100000 * f.d + 1 AS n \
         FROM (SELECT 0 d UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9) a, \
         (SELECT 0 d UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9) b, \
         (SELECT 0 d UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9) c, \
         (SELECT 0 d UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9) d, \
         (SELECT 0 d UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9) e, \
         (SELECT 0 d UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9) f)"
    }
}

impl SqlFixture for MysqlFixture {
    fn label(&self) -> &str {
        if self.mariadb {
            "mariadb"
        } else {
            "mysql"
        }
    }
    fn driver_id(&self) -> &str {
        self.label()
    }
    fn identifier_quote(&self) -> &str {
        "`"
    }
    fn supports_schemas(&self) -> bool {
        false
    }
    fn admin_uri(&self, uri: &str) -> Option<String> {
        // The test containers' root account (password `tablex`), unless overridden.
        let var = if self.mariadb {
            "TABLEX_TEST_MARIADB_ROOT_URI"
        } else {
            "TABLEX_TEST_MYSQL_ROOT_URI"
        };
        std::env::var(var).ok().or_else(|| {
            let (scheme, rest) = uri.split_once("://")?;
            let host = rest.rsplit_once('@').map_or(rest, |(_, host)| host);
            Some(format!("{scheme}://root:tablex@{host}"))
        })
    }
    fn admin_setup(&self, uri: &str) -> Vec<String> {
        let user = uri
            .split_once("://")
            .and_then(|(_, rest)| rest.rsplit_once('@'))
            .map_or("tablex", |(userinfo, _)| {
                userinfo.split_once(':').map_or(userinfo, |(user, _)| user)
            });
        ["cf", "cfdb", "cfconfirm"]
            .iter()
            .map(|prefix| format!("GRANT ALL PRIVILEGES ON `{prefix}\\_%`.* TO '{user}'@'%'"))
            .collect()
    }
    fn current_database_query(&self) -> String {
        "SELECT DATABASE()".into()
    }
    fn create_scratch(&self, scratch: &Scratch) -> Vec<String> {
        vec![format!("CREATE DATABASE {}", scratch.name)]
    }
    fn drop_scratch(&self, scratch: &Scratch) -> Vec<String> {
        vec![format!("DROP DATABASE IF EXISTS {}", scratch.name)]
    }
    fn create_tables(&self, scratch: &Scratch) -> Vec<String> {
        let s = &scratch.name;
        vec![
            format!("CREATE TABLE {s}.items (tenant INT NOT NULL, id INT NOT NULL, name VARCHAR(100) NULL, qty INT NULL, PRIMARY KEY (tenant, id)) ENGINE=InnoDB"),
            format!("CREATE TABLE {s}.refs (id INT PRIMARY KEY, tenant INT NOT NULL, item_id INT NOT NULL, FOREIGN KEY (tenant, item_id) REFERENCES {s}.items (tenant, id)) ENGINE=InnoDB"),
            format!("CREATE TABLE {s}.notes (owner VARCHAR(20) NULL, body VARCHAR(50) NULL) ENGINE=InnoDB"),
            format!("CREATE TABLE {s}.dups (k INT, v INT) ENGINE=InnoDB"),
        ]
    }
    fn table(&self, scratch: &Scratch, table: &str) -> String {
        format!("{}.{table}", scratch.name)
    }
    fn numbers(&self, n: u64) -> String {
        format!(
            "SELECT n FROM {} AS m WHERE n <= {n} ORDER BY n",
            Self::million()
        )
    }
    fn slow_query(&self, seconds: u32) -> String {
        // `SLEEP()` returns 1 instead of failing when interrupted, so burn CPU in
        // a join instead, bounded on the server by `seconds`.
        let join = format!(
            "SELECT COUNT(*) FROM {} AS x, {} AS y",
            Self::million(),
            Self::million()
        );
        if self.mariadb {
            format!("SET STATEMENT max_statement_time={seconds} FOR {join}")
        } else {
            let millis = u64::from(seconds) * 1000;
            join.replacen(
                "SELECT",
                &format!("SELECT /*+ MAX_EXECUTION_TIME({millis}) */"),
                1,
            )
        }
    }
    fn backend_id_query(&self) -> String {
        "SELECT CONNECTION_ID()".into()
    }
    fn interrupt_backend_statement(&self, id: &str) -> String {
        format!("KILL QUERY {id}")
    }
    fn begin(&self) -> &str {
        "START TRANSACTION"
    }
    fn write_statements(&self, scratch: &Scratch) -> Vec<String> {
        let t = self.table(scratch, "items");
        vec![
            format!("INSERT INTO {t} (tenant, id, name, qty) VALUES (9, 1, 'w1', 1)"),
            format!("INSERT INTO {t} (tenant, id, name, qty) VALUES (9, 2, 'w2', 2)"),
            format!("INSERT INTO {t} (tenant, id, name, qty) VALUES (9, 3, 'w3', 3)"),
        ]
    }
    fn write_returning_statements(&self, scratch: &Scratch) -> Vec<String> {
        if !self.mariadb {
            return Vec::new();
        }
        let t = self.table(scratch, "items");
        vec![format!(
            "INSERT INTO {t} (tenant, id, name, qty) VALUES (8, 1, 'r1', 1) RETURNING tenant, id"
        )]
    }
    fn grant_statement(&self, scratch: &Scratch) -> String {
        format!(
            "GRANT SELECT ON {} TO 'cf_nobody'@'localhost'",
            self.table(scratch, "items")
        )
    }
    fn returning_write(&self, scratch: &Scratch, tenant: u32) -> Option<String> {
        let t = self.table(scratch, "items");
        let insert = format!(
            "INSERT INTO {t} (tenant, id, name, qty) VALUES ({tenant}, 1, 'a', 1), ({tenant}, 2, 'b', 2), ({tenant}, 3, 'c', 3)"
        );
        Some(if self.mariadb {
            format!("{insert} RETURNING tenant, id")
        } else {
            // MySQL has no RETURNING: a script whose last statement selects.
            format!("{insert}; SELECT tenant, id FROM {t} WHERE tenant = {tenant} ORDER BY id")
        })
    }
    fn bulk_object_statements(&self, scratch: &Scratch, count: u32) -> Option<Vec<String>> {
        let s = &scratch.name;
        // MEMORY tables are created in microseconds (InnoDB would take minutes).
        Some(vec![
            format!("CREATE PROCEDURE {s}.cf_fill() BEGIN DECLARE i INT DEFAULT 0; WHILE i < {count} DO SET @cf_sql = CONCAT('CREATE TABLE {s}.b', i, ' (c INT) ENGINE=MEMORY'); PREPARE cf_st FROM @cf_sql; EXECUTE cf_st; DEALLOCATE PREPARE cf_st; SET i = i + 1; END WHILE; END"),
            format!("CALL {s}.cf_fill()"),
        ])
    }
    fn slow_mutation_setup_as_admin(&self) -> bool {
        true
    }
    fn slow_mutation_setup(&self, scratch: &Scratch, uri: &str) -> Option<Vec<String>> {
        let s = &scratch.name;
        let user = Self::uri_user(uri);
        // A loop interrupts with ER_QUERY_INTERRUPTED (SLEEP() would just return
        // 1); it ends by itself after 20 s. Creating triggers needs SUPER on
        // servers with binary logging, so this runs as administrator (the trigger
        // is defined as the test user, so the driver may `KILL QUERY` it).
        Some(vec![
            format!("CREATE TABLE {s}.slow (id INT) ENGINE=InnoDB"),
            format!("CREATE DEFINER = '{user}'@'%' TRIGGER {s}.slow_insert BEFORE INSERT ON {s}.slow FOR EACH ROW BEGIN DECLARE i BIGINT DEFAULT 0; DECLARE t DATETIME DEFAULT SYSDATE(); WHILE TIMESTAMPDIFF(SECOND, t, SYSDATE()) < 20 DO SET i = i + 1; END WHILE; END"),
        ])
    }
    fn navigator_tables_request(&self, scratch: &Scratch) -> Value {
        json!({
            "parent": null,
            "database": scratch.name,
            "payload": {"database": scratch.name},
            "cursor": null,
            "limit": 0
        })
    }
    fn object_ref(&self, scratch: &Scratch, table: &str) -> SqlObjectRef {
        SqlObjectRef {
            database: Some(scratch.name.clone()),
            schema: None,
            object: table.into(),
        }
    }
    fn mutation_target(&self, scratch: &Scratch, table: &str) -> MutationTarget {
        MutationTarget {
            database: Some(scratch.name.clone()),
            object: table.into(),
            identity: json!({}),
        }
    }
    fn drop_database(&self, name: &str) -> Vec<String> {
        vec![format!("DROP DATABASE IF EXISTS {name}")]
    }
}

/// SQLite: one database file, no schemas; the scratch is just the four tables
/// (dropped afterwards). The connection URI must point at a *file* (the suite
/// opens several connections, which an in-memory database cannot share).
#[derive(Debug, Default, Clone, Copy)]
pub struct SqliteFixture;

impl SqlFixture for SqliteFixture {
    fn label(&self) -> &str {
        "sqlite"
    }
    fn driver_id(&self) -> &str {
        "sqlite"
    }
    fn identifier_quote(&self) -> &str {
        "\""
    }
    fn supports_schemas(&self) -> bool {
        false
    }
    /// Not applicable: a SQLite database is a file, there is no `CREATE DATABASE`.
    fn supports_create_database(&self) -> bool {
        false
    }
    /// Not applicable: there is no SQL to interrupt another connection's statement
    /// (`sqlite3_interrupt` is a C API on the connection that runs it).
    fn supports_external_interrupt(&self) -> bool {
        false
    }
    fn current_database_query(&self) -> String {
        "SELECT 'main'".into()
    }
    fn create_scratch(&self, _: &Scratch) -> Vec<String> {
        Vec::new()
    }
    fn drop_scratch(&self, _: &Scratch) -> Vec<String> {
        ["swallow", "slow", "refs", "items", "notes", "dups"]
            .iter()
            .map(|table| format!("DROP TABLE IF EXISTS {table}"))
            .collect()
    }
    fn create_tables(&self, _: &Scratch) -> Vec<String> {
        vec![
            "CREATE TABLE items (tenant INT NOT NULL, id INT NOT NULL, name VARCHAR(100) NULL, qty INT NULL, PRIMARY KEY (tenant, id))".into(),
            "CREATE TABLE refs (id INT PRIMARY KEY, tenant INT NOT NULL, item_id INT NOT NULL, FOREIGN KEY (tenant, item_id) REFERENCES items (tenant, id))".into(),
            "CREATE TABLE notes (owner VARCHAR(20) NULL, body VARCHAR(50) NULL)".into(),
            "CREATE TABLE dups (k INT, v INT)".into(),
        ]
    }
    fn table(&self, _: &Scratch, table: &str) -> String {
        table.into()
    }
    fn numbers(&self, n: u64) -> String {
        format!(
            "WITH RECURSIVE s(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM s WHERE n < {n}) \
             SELECT n FROM s ORDER BY n"
        )
    }
    fn slow_query(&self, seconds: u32) -> String {
        // A counting CTE of `seconds` x 20M rows: interruptible by
        // `sqlite3_interrupt`, and finite on its own (about `seconds` s).
        let rows = u64::from(seconds) * 20_000_000;
        format!(
            "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < {rows}) \
             SELECT count(*) FROM c"
        )
    }
    fn backend_id_query(&self) -> String {
        "SELECT 1".into()
    }
    fn interrupt_backend_statement(&self, _: &str) -> String {
        "SELECT 1".into()
    }
    fn write_statements(&self, _: &Scratch) -> Vec<String> {
        vec![
            "INSERT INTO items (tenant, id, name, qty) VALUES (9, 1, 'w1', 1)".into(),
            "INSERT INTO items (tenant, id, name, qty) VALUES (9, 2, 'w2', 2)".into(),
            "INSERT INTO items (tenant, id, name, qty) VALUES (9, 3, 'w3', 3)".into(),
        ]
    }
    fn write_returning_statements(&self, _: &Scratch) -> Vec<String> {
        vec![
            "INSERT INTO items (tenant, id, name, qty) VALUES (8, 1, 'r1', 1) RETURNING tenant, id"
                .into(),
        ]
    }
    fn grant_statement(&self, _: &Scratch) -> String {
        "GRANT SELECT ON items TO nobody".into()
    }
    fn slow_mutation_setup(&self, _: &Scratch, _uri: &str) -> Option<Vec<String>> {
        Some(vec![
            "CREATE TABLE slow (id INT)".into(),
            "CREATE TRIGGER slow_insert BEFORE INSERT ON slow BEGIN SELECT count(*) FROM (WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 400000000) SELECT x FROM c); END".into(),
        ])
    }
    fn bulk_object_statements(&self, _: &Scratch, count: u32) -> Option<Vec<String>> {
        // One transaction: thousands of CREATE TABLEs would otherwise each sync.
        let mut statements = vec!["BEGIN".to_owned()];
        statements.extend((1..=count).map(|i| format!("CREATE TABLE b{i} (c INT)")));
        statements.push("COMMIT".into());
        Some(statements)
    }
    fn bulk_object_cleanup(&self, _: &Scratch, count: u32) -> Vec<String> {
        let mut statements = vec!["BEGIN".to_owned()];
        statements.extend((1..=count).map(|i| format!("DROP TABLE IF EXISTS b{i}")));
        statements.push("COMMIT".into());
        statements
    }
    fn returning_write(&self, _: &Scratch, tenant: u32) -> Option<String> {
        Some(format!(
            "INSERT INTO items (tenant, id, name, qty) VALUES ({tenant}, 1, 'a', 1), ({tenant}, 2, 'b', 2), ({tenant}, 3, 'c', 3) RETURNING tenant, id"
        ))
    }
    fn zero_row_insert_setup(&self, _: &Scratch) -> Option<Vec<String>> {
        Some(vec![
            "CREATE TABLE swallow (id INT)".into(),
            "CREATE TRIGGER swallow_ignore BEFORE INSERT ON swallow BEGIN SELECT RAISE(IGNORE); END"
                .into(),
        ])
    }
    fn navigator_tables_request(&self, _: &Scratch) -> Value {
        json!({
            "parent": "database:main",
            "database": "main",
            "payload": {"database": "main"},
            "cursor": null,
            "limit": 0
        })
    }
    fn object_ref(&self, _: &Scratch, table: &str) -> SqlObjectRef {
        SqlObjectRef {
            database: Some("main".into()),
            schema: None,
            object: table.into(),
        }
    }
    fn mutation_target(&self, _: &Scratch, table: &str) -> MutationTarget {
        MutationTarget {
            database: None,
            object: table.into(),
            identity: json!({}),
        }
    }
    fn drop_database(&self, _: &str) -> Vec<String> {
        Vec::new()
    }
}

/// ClickHouse: the scratch is a database. There are no transactions, foreign
/// keys or data-modifying CTEs; `items` is created with the block-number column
/// lightweight `UPDATE` needs, and `notes`/`dups` have a sorting key because
/// the driver edits rows only in MergeTree tables that have one.
#[derive(Debug, Default, Clone, Copy)]
pub struct ClickHouseFixture;

impl SqlFixture for ClickHouseFixture {
    fn label(&self) -> &str {
        "clickhouse"
    }
    fn driver_id(&self) -> &str {
        "clickhouse"
    }
    fn identifier_quote(&self) -> &str {
        "`"
    }
    fn supports_schemas(&self) -> bool {
        false
    }
    fn supports_transactions(&self) -> bool {
        false
    }
    fn supports_foreign_keys(&self) -> bool {
        false
    }
    fn delete_all(&self, table: &str) -> String {
        format!("TRUNCATE TABLE {table}")
    }
    fn current_database_query(&self) -> String {
        "SELECT currentDatabase()".into()
    }
    fn create_scratch(&self, scratch: &Scratch) -> Vec<String> {
        vec![format!("CREATE DATABASE {}", scratch.name)]
    }
    fn drop_scratch(&self, scratch: &Scratch) -> Vec<String> {
        vec![format!("DROP DATABASE IF EXISTS {} SYNC", scratch.name)]
    }
    fn create_tables(&self, scratch: &Scratch) -> Vec<String> {
        let s = &scratch.name;
        vec![
            format!("CREATE TABLE {s}.items (tenant Int32, id Int32, name Nullable(String), qty Nullable(Int32)) ENGINE = MergeTree ORDER BY (tenant, id) SETTINGS enable_block_number_column = 1, enable_block_offset_column = 1"),
            format!("CREATE TABLE {s}.refs (id Int32, tenant Int32, item_id Int32) ENGINE = MergeTree ORDER BY id"),
            format!("CREATE TABLE {s}.notes (owner Nullable(String), body Nullable(String)) ENGINE = MergeTree ORDER BY owner SETTINGS allow_nullable_key = 1"),
            format!("CREATE TABLE {s}.dups (k Int32, v Int32) ENGINE = MergeTree ORDER BY k"),
        ]
    }
    fn table(&self, scratch: &Scratch, table: &str) -> String {
        format!("{}.{table}", scratch.name)
    }
    fn numbers(&self, n: u64) -> String {
        format!("SELECT number + 1 AS n FROM numbers({n}) ORDER BY n")
    }
    fn slow_query(&self, seconds: u32) -> String {
        // A scan that cannot finish in time and prints nothing until it does;
        // the server stops it after `seconds` on its own.
        format!(
            "SELECT count() FROM numbers(1000000000000) WHERE cityHash64(number) % 1000003 = 7 \
             SETTINGS max_execution_time = {seconds}, max_threads = 2"
        )
    }
    /// The connection's query-id token: every query id of one connection is
    /// `tablex-<token>-<operation>-<statement>`.
    fn backend_id_query(&self) -> String {
        "SELECT splitByChar('-', queryID())[2]".into()
    }
    fn interrupt_backend_statement(&self, id: &str) -> String {
        format!("KILL QUERY WHERE startsWith(query_id, 'tablex-{id}-')")
    }
    fn write_statements(&self, scratch: &Scratch) -> Vec<String> {
        let t = self.table(scratch, "items");
        vec![
            format!("INSERT INTO {t} (tenant, id, name, qty) VALUES (9, 1, 'w1', 1)"),
            format!("INSERT INTO {t} (tenant, id, name, qty) VALUES (9, 2, 'w2', 2)"),
            format!("INSERT INTO {t} (tenant, id, name, qty) VALUES (9, 3, 'w3', 3)"),
        ]
    }
    /// No `INSERT ... RETURNING` and no data-modifying CTE.
    fn write_returning_statements(&self, _: &Scratch) -> Vec<String> {
        Vec::new()
    }
    fn grant_statement(&self, scratch: &Scratch) -> String {
        format!("GRANT SELECT ON {} TO cf_nobody", self.table(scratch, "items"))
    }
    fn returning_write(&self, scratch: &Scratch, tenant: u32) -> Option<String> {
        let t = self.table(scratch, "items");
        Some(format!(
            "INSERT INTO {t} (tenant, id, name, qty) VALUES ({tenant}, 1, 'a', 1), ({tenant}, 2, 'b', 2), ({tenant}, 3, 'c', 3); \
             SELECT tenant, id FROM {t} WHERE tenant = {tenant} ORDER BY id"
        ))
    }
    fn bulk_object_statements(&self, scratch: &Scratch, count: u32) -> Option<Vec<String>> {
        let s = &scratch.name;
        // Memory tables are created in a few milliseconds; scripts of 500
        // statements keep the number of requests the suite makes small. The
        // scratch database is dropped as a whole afterwards.
        Some(
            (1..=count)
                .step_by(500)
                .map(|from| {
                    (from..(from + 500).min(count + 1))
                        .map(|i| format!("CREATE TABLE {s}.b{i} (c UInt8) ENGINE = Memory"))
                        .collect::<Vec<_>>()
                        .join(";\n")
                })
                .collect(),
        )
    }
    // `slow_mutation_setup` stays `None` (the `mutation_cancelled` skip): an
    // INSERT can only be slowed through materialized views or sleeping
    // functions, and ClickHouse observes `KILL QUERY` there only at block
    // boundaries (measured: a killed insert through 6 x `sleep(3)` views still
    // ran 18 s), so no insert is both long and interruptible.
    fn navigator_tables_request(&self, scratch: &Scratch) -> Value {
        json!({
            "parent": null,
            "database": scratch.name,
            "payload": {"database": scratch.name},
            "cursor": null,
            "limit": 0
        })
    }
    fn object_ref(&self, scratch: &Scratch, table: &str) -> SqlObjectRef {
        SqlObjectRef {
            database: Some(scratch.name.clone()),
            schema: None,
            object: table.into(),
        }
    }
    fn mutation_target(&self, scratch: &Scratch, table: &str) -> MutationTarget {
        MutationTarget {
            database: Some(scratch.name.clone()),
            object: table.into(),
            identity: json!({}),
        }
    }
    fn drop_database(&self, name: &str) -> Vec<String> {
        vec![format!("DROP DATABASE IF EXISTS {name} SYNC")]
    }
}
