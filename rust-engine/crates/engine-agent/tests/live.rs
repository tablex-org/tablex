//! Live tests against the dedicated test containers, through the real
//! drivers. Ignored by default and gated on the environment:
//!
//! ```text
//! TABLEX_TEST_PG_URI=postgres://tablex:tablex@127.0.0.1:55432/tablex \
//! TABLEX_TEST_MYSQL_URI=mysql://tablex:tablex@127.0.0.1:53306/tablex \
//! TABLEX_TEST_REDIS_URI=redis://127.0.0.1:56379 \
//! TABLEX_TEST_MONGO_URI=mongodb://127.0.0.1:57017 \
//! cargo test --manifest-path rust-engine/Cargo.toml -p engine-agent --test live -- --ignored
//! ```
//!
//! Only uniquely named scratch tables / prefixed keys are created, and they
//! are dropped / deleted even when a test fails. One MongoDB test only reads
//! (`admin.system.version` and documents the server generates itself); the other writes, in a
//! uniquely named database that is dropped when it ends, even when it fails.

mod common;

use common::{load_driver, DriverProvider};
use engine_agent::schema::{spec, validate};
use engine_agent::testing::{connection, FakeCatalog, ScriptedApprover};
use engine_agent::{
    AgentAccess, AgentError, ApprovalDecision, CancelToken, ConnectionKind, Language, Limits,
    Session, ToolLayer, ToolName, ToolOutcome,
};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

const ID: &str = "live";

fn unique(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!(
        "{prefix}_{}_{}",
        std::process::id(),
        nanos % 1_000_000_000_000
    )
}

fn code(outcome: &ToolOutcome) -> &str {
    outcome.code().unwrap_or("ok")
}

struct Live {
    layer: Arc<ToolLayer>,
    catalog: Arc<FakeCatalog>,
    approver: Arc<ScriptedApprover>,
    raw: Arc<dyn Session>,
    kind: ConnectionKind,
    /// Statements to run (outside the policy) when the test ends, with their database.
    cleanup: Vec<(String, Option<String>)>,
}

impl Live {
    fn new(kind: ConnectionKind, package: &'static str, uri: &str, access: AgentAccess) -> Self {
        let features = if package == "dbengine-clickhouse" {
            ""
        } else {
            "abi-v2"
        };
        let handle = load_driver(package, features);
        let provider = DriverProvider::new(handle, &[(ID, uri)]);
        let raw = provider.raw_session(ID);
        let catalog = FakeCatalog::new(vec![connection(ID, kind, Some(access))]);
        let approver = ScriptedApprover::new(ApprovalDecision::Approve, true);
        let layer = Arc::new(
            ToolLayer::new(catalog.clone(), provider, "live-test")
                .with_transactions()
                .with_approver(approver.clone()),
        );
        Self {
            layer,
            catalog,
            approver,
            raw,
            kind,
            cleanup: Vec::new(),
        }
    }

    fn call(&self, tool: &str, args: Value) -> ToolOutcome {
        let outcome = self.layer.call(tool, args);
        if let (ToolOutcome::Success(value), Some(tool)) = (&outcome, ToolName::parse(tool)) {
            validate(&spec(tool).output_schema, value).unwrap_or_else(|error| {
                panic!("{tool:?} output breaks its schema: {error}\n{value}")
            });
        }
        outcome
    }

    fn ok(&self, tool: &str, args: Value) -> Value {
        match self.call(tool, args) {
            ToolOutcome::Success(value) => value,
            ToolOutcome::Failure(error) => panic!("{tool} failed: {error:?}"),
        }
    }

    /// Runs `text` outside the policy (setup and cleanup only).
    fn raw(&self, language: Language, text: &str, database: Option<&str>) {
        let limits = Limits::default();
        let _ = self.raw.run(
            &engine_agent::ports::RunRequest {
                language,
                text,
                database,
                limits: &limits,
                row_limit: 10,
                confirmed: true,
                read_only: false,
            },
            &CancelToken::new(),
        );
    }

    /// Runs `text` outside the policy and the function denylist, as if a
    /// statement had slipped past both; `read_only` asks the driver to run it read-only.
    fn raw_run(
        &self,
        text: &str,
        database: Option<&str>,
        read_only: bool,
    ) -> Result<engine_agent::result::QueryResult, AgentError> {
        let limits = Limits::default();
        self.raw.run(
            &engine_agent::ports::RunRequest {
                language: Language::Sql,
                text,
                database,
                limits: &limits,
                row_limit: 10,
                confirmed: true,
                read_only,
            },
            &CancelToken::new(),
        )
    }

    fn sql(&self, statement: &str, extra: Value) -> ToolOutcome {
        let mut args = json!({"connection_id": ID, "statement": statement});
        for (key, value) in extra.as_object().into_iter().flatten() {
            args[key] = value.clone();
        }
        self.call("execute_query", args)
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        let language = if self.kind == ConnectionKind::Redis {
            Language::Redis
        } else {
            Language::Sql
        };
        for (statement, database) in std::mem::take(&mut self.cleanup) {
            self.raw(language, &statement, database.as_deref());
        }
        self.layer.close_all();
        self.raw.disconnect();
    }
}

fn database_of(uri: &str) -> String {
    uri.rsplit('/')
        .next()
        .unwrap()
        .split('?')
        .next()
        .unwrap()
        .to_owned()
}

/// The access/statement matrix on a SQL connection, shared by PostgreSQL and
/// MySQL. `table` is a scratch table the test created.
fn sql_policy_matrix(live: &Live, table: &str, database: Option<&str>) {
    let mut extra = json!({});
    if let Some(database) = database {
        extra["database"] = json!(database);
    }
    // Read: reads run, every kind of write is refused, nothing changes.
    live.catalog
        .update(ID, |info| info.agent_access = Some(AgentAccess::Read));
    let before = live.sql(&format!("SELECT count(*) FROM {table}"), extra.clone());
    assert_eq!(code(&before), "ok", "{before:?}");
    let count = |outcome: &ToolOutcome| match outcome {
        ToolOutcome::Success(value) => value["rows"][0][0].as_str().unwrap().to_owned(),
        ToolOutcome::Failure(error) => panic!("{error:?}"),
    };
    let rows_before = count(&before);
    for statement in [
        format!("INSERT INTO {table}(id, name) VALUES (900, 'nope')"),
        format!("UPDATE {table} SET name = 'x' WHERE id = 1"),
        format!("DELETE FROM {table} WHERE id = 1"),
        format!("TRUNCATE {table}"),
        format!("DROP TABLE {table}"),
        format!("WITH d AS (DELETE FROM {table} WHERE id = 1 RETURNING id) SELECT * FROM d"),
        format!("SELECT * INTO {table}_copy FROM {table}"),
        format!("SELECT 1; DELETE FROM {table}"),
        format!("EXPLAIN ANALYZE DELETE FROM {table} WHERE id = 1"),
    ] {
        let outcome = live.sql(&statement, extra.clone());
        assert!(
            matches!(
                code(&outcome),
                "agent.write_refused" | "agent.multi_statement"
            ),
            "{statement}: {outcome:?}"
        );
    }
    assert_eq!(
        count(&live.sql(&format!("SELECT count(*) FROM {table}"), extra.clone())),
        rows_before
    );
    // Schema only: no data at all.
    live.catalog
        .update(ID, |info| info.agent_access = Some(AgentAccess::SchemaOnly));
    assert_eq!(
        code(&live.sql(&format!("SELECT * FROM {table}"), extra.clone())),
        "agent.access_denied"
    );
    // Read + write: an approved write runs, a dangerous one needs the flag.
    live.catalog
        .update(ID, |info| info.agent_access = Some(AgentAccess::ReadWrite));
    let mut with_flag = extra.clone();
    with_flag["dangerous"] = json!(true);
    let inserted = live.sql(
        &format!("INSERT INTO {table}(id, name) VALUES (901, 'added')"),
        extra.clone(),
    );
    assert_eq!(code(&inserted), "ok", "{inserted:?}");
    assert_eq!(
        live.layer
            .call(
                "execute_query",
                json!({"connection_id": ID, "statement": format!("DELETE FROM {table}")})
            )
            .code(),
        Some("agent.dangerous_refused")
    );
    let wiped = live.sql(&format!("DELETE FROM {table}"), with_flag);
    assert_eq!(code(&wiped), "ok", "{wiped:?}");
    assert!(live
        .approver
        .asked
        .lock()
        .unwrap()
        .iter()
        .any(|(statement, danger)| statement.starts_with("DELETE") && danger.is_some()));
    assert_eq!(
        count(&live.sql(&format!("SELECT count(*) FROM {table}"), extra)),
        "0"
    );
}

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI"]
fn postgresql_tools_and_policy() {
    let Ok(uri) = std::env::var("TABLEX_TEST_PG_URI") else {
        return;
    };
    let database = database_of(&uri);
    let mut live = Live::new(
        ConnectionKind::PostgreSql,
        "dbengine-postgres",
        &uri,
        AgentAccess::ReadWrite,
    );
    let parent = unique("tablex_agent_parent");
    let child = unique("tablex_agent_child");
    let policy = unique("tablex_agent_policy");
    live.cleanup = vec![
        (format!("DROP TABLE IF EXISTS {policy}"), None),
        (format!("DROP TABLE IF EXISTS {child}"), None),
        (format!("DROP TABLE IF EXISTS {parent}"), None),
    ];

    let opened = live.ok("open_session", json!({"connection_id": ID}));
    assert_eq!(opened["supports_schemas"], true);
    let databases = live.ok("list_namespaces", json!({"connection_id": ID}));
    assert!(
        databases.to_string().contains(&format!("\"{database}\"")),
        "{databases}"
    );

    for statement in [
        format!("CREATE TABLE {parent}(id integer PRIMARY KEY, name text NOT NULL)"),
        format!("COMMENT ON COLUMN {parent}.name IS 'display name'"),
        format!("CREATE TABLE {child}(id serial PRIMARY KEY, parent_id integer NOT NULL REFERENCES {parent}(id) ON DELETE CASCADE, note text)"),
        format!("INSERT INTO {parent}(id, name) VALUES (1, 'one'), (2, 'two'), (3, 'three')"),
    ] {
        let outcome = live.sql(&statement, json!({}));
        assert_eq!(code(&outcome), "ok", "{statement}: {outcome:?}");
    }
    // Schemas that hold objects are listed.
    let schemas = live.ok(
        "list_namespaces",
        json!({"connection_id": ID, "database": database}),
    );
    assert!(
        schemas["namespaces"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == "public"),
        "{schemas}"
    );
    // Without the schema, the call says what is missing.
    assert_eq!(
        code(&live.call(
            "list_tables",
            json!({"connection_id": ID, "database": database})
        )),
        "agent.invalid_arguments"
    );
    let ns = json!({"connection_id": ID, "database": database, "schema": "public"});
    let mut listing = ns.clone();
    listing["filter"] = json!("tablex_agent");
    let tables = live.ok("list_tables", listing);
    let names: Vec<&str> = tables["tables"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&parent.as_str()) && names.contains(&child.as_str()),
        "{names:?}"
    );

    let mut describe = ns.clone();
    describe["table"] = json!(child);
    let child_description = live.ok("describe_table", describe);
    assert_eq!(child_description["primary_key"], json!(["id"]));
    assert!(
        child_description["foreign_keys"][0]["referenced_table"]
            .as_str()
            .unwrap()
            .ends_with(&parent),
        "{child_description}"
    );
    let mut describe = ns.clone();
    describe["table"] = json!(parent);
    let parent_description = live.ok("describe_table", describe);
    assert_eq!(parent_description["columns"][1]["comment"], "display name");

    let mut context = ns.clone();
    context["budget"] = json!(200_000);
    let digest = live.ok("get_schema_context", context.clone());
    let text = digest["text"].as_str().unwrap();
    assert!(
        text.contains(&format!("TABLE {parent}\n  id integer NOT NULL PK")),
        "{text}"
    );
    assert!(
        text.contains("name text NOT NULL  -- display name"),
        "{text}"
    );
    assert!(
        text.contains(&format!("TABLE {child}")) && text.contains("ON DELETE CASCADE"),
        "{text}"
    );
    assert_eq!(
        live.ok("get_schema_context", context.clone()),
        digest,
        "deterministic"
    );
    context["budget"] = json!(150);
    let tight = live.ok("get_schema_context", context);
    assert_eq!(tight["within_budget"], true);
    assert!(tight["tables_included"].as_u64().unwrap() <= tight["tables_total"].as_u64().unwrap());

    let rows = live.sql(
        &format!("SELECT id, name FROM {parent} ORDER BY id"),
        json!({}),
    );
    match &rows {
        ToolOutcome::Success(value) => assert_eq!(
            value["rows"],
            json!([["1", "one"], ["2", "two"], ["3", "three"]])
        ),
        ToolOutcome::Failure(error) => panic!("{error:?}"),
    }
    let plan = live.ok(
        "explain_query",
        json!({"connection_id": ID, "statement": format!("SELECT * FROM {parent} WHERE id = 2")}),
    );
    assert!(plan["rows"].to_string().contains(&parent), "{plan}");
    let series = live.ok("execute_query", json!({"connection_id": ID, "statement": "SELECT g FROM generate_series(1, 5000) g", "row_limit": 5000}));
    assert_eq!(series["row_count"], 1000);
    assert_eq!(series["truncated"], true);
    // A driver error passes through with its code.
    let broken = live.sql("SELECT * FROM table_that_is_not_there", json!({}));
    assert!(
        broken.is_error() && !code(&broken).starts_with("agent."),
        "{broken:?}"
    );
    // A slow statement hits the connection's timeout.
    live.catalog
        .update(ID, |info| info.statement_timeout_secs = Some(1));
    let slow = live.sql("SELECT pg_sleep(30)", json!({}));
    assert_eq!(code(&slow), "agent.limit", "{slow:?}");
    live.catalog
        .update(ID, |info| info.statement_timeout_secs = None);
    assert_eq!(code(&live.sql("SELECT 1", json!({}))), "ok");

    let made = live.sql(
        &format!("CREATE TABLE {policy}(id integer PRIMARY KEY, name text)"),
        json!({}),
    );
    assert_eq!(code(&made), "ok", "{made:?}");
    let seeded = live.sql(
        &format!("INSERT INTO {policy}(id, name) VALUES (1, 'a'), (2, 'b')"),
        json!({}),
    );
    assert_eq!(code(&seeded), "ok", "{seeded:?}");
    sql_policy_matrix(&live, &policy, None);
    // Dropping needs the dangerous flag, then the approver.
    assert_eq!(
        code(&live.sql(&format!("DROP TABLE {policy}"), json!({}))),
        "agent.dangerous_refused"
    );
    assert_eq!(
        code(&live.sql(&format!("DROP TABLE {policy}"), json!({"dangerous": true}))),
        "ok"
    );
    assert_eq!(
        code(&live.sql(&format!("DROP TABLE {child}"), json!({"dangerous": true}))),
        "ok"
    );
    assert_eq!(
        code(&live.sql(&format!("DROP TABLE {parent}"), json!({"dangerous": true}))),
        "ok"
    );
}

#[test]
#[ignore = "needs TABLEX_TEST_MYSQL_URI"]
fn mysql_tools_and_policy() {
    mysql_family(
        ConnectionKind::MySql,
        "dbengine-mysql",
        "TABLEX_TEST_MYSQL_URI",
    );
}

#[test]
#[ignore = "needs TABLEX_TEST_MARIADB_URI"]
fn mariadb_tools_and_policy() {
    mysql_family(
        ConnectionKind::MariaDb,
        "dbengine-mariadb",
        "TABLEX_TEST_MARIADB_URI",
    );
}

#[test]
#[ignore = "needs TABLEX_TEST_MYSQL_URI"]
fn mysql_adapter_lexes_a_session_by_its_own_sql_mode() {
    sql_mode_decides_the_adapters_lexer(
        ConnectionKind::MySql,
        "dbengine-mysql",
        "TABLEX_TEST_MYSQL_URI",
        "TABLEX_TEST_MYSQL_ROOT_URI",
    );
}

#[test]
#[ignore = "needs TABLEX_TEST_MARIADB_URI"]
fn mariadb_adapter_lexes_a_session_by_its_own_sql_mode() {
    sql_mode_decides_the_adapters_lexer(
        ConnectionKind::MariaDb,
        "dbengine-mariadb",
        "TABLEX_TEST_MARIADB_URI",
        "TABLEX_TEST_MARIADB_ROOT_URI",
    );
}

/// `DriverSessionAdapter::new` reads the connection's own dialect (the connect reply),
/// not the driver's static one: a session opened while the server's GLOBAL `sql_mode`
/// has `ANSI_QUOTES` lexes `"…"` as an identifier, one opened before does not.
fn sql_mode_decides_the_adapters_lexer(
    kind: ConnectionKind,
    package: &'static str,
    env: &str,
    root_env: &str,
) {
    let Ok(uri) = std::env::var(env) else {
        return;
    };
    let root = std::env::var(root_env).unwrap_or_else(|_| {
        let (scheme, rest) = uri.split_once("://").expect("URI scheme");
        let host = rest.rsplit_once('@').map_or(rest, |(_, host)| host);
        format!("{scheme}://root:tablex@{host}")
    });
    let lex = |live: &Live| live.raw.info().lex.expect("a SQL lexer");
    let default = Live::new(kind, package, &uri, AgentAccess::ReadWrite);
    assert!(!lex(&default).double_quote_is_identifier);

    // The root session keeps the previous mode in a session variable and puts it back on drop.
    let mut admin = Live::new(kind, package, &root, AgentAccess::ReadWrite);
    admin.cleanup = vec![("SET GLOBAL sql_mode = @tablex_prev_mode".into(), None)];
    admin.raw(
        Language::Sql,
        "SET @tablex_prev_mode = @@GLOBAL.sql_mode",
        None,
    );
    admin.raw(
        Language::Sql,
        "SET GLOBAL sql_mode = CONCAT_WS(',', NULLIF(@@GLOBAL.sql_mode, ''), 'ANSI_QUOTES')",
        None,
    );
    let ansi = Live::new(kind, package, &uri, AgentAccess::ReadWrite);
    assert!(lex(&ansi).double_quote_is_identifier);
    assert!(lex(&ansi).backslash_escapes, "the backslash mode is unchanged");
    // The session opened earlier keeps the reading it connected with.
    assert!(!lex(&default).double_quote_is_identifier);
}

fn mysql_family(kind: ConnectionKind, package: &'static str, env: &str) {
    let Ok(uri) = std::env::var(env) else {
        return;
    };
    let database = database_of(&uri);
    let mut live = Live::new(kind, package, &uri, AgentAccess::ReadWrite);
    let parent = unique("tablex_agent_parent");
    let child = unique("tablex_agent_child");
    let policy = unique("tablex_agent_policy");
    live.cleanup = vec![
        (
            format!("DROP TABLE IF EXISTS {policy}"),
            Some(database.clone()),
        ),
        (
            format!("DROP TABLE IF EXISTS {child}"),
            Some(database.clone()),
        ),
        (
            format!("DROP TABLE IF EXISTS {parent}"),
            Some(database.clone()),
        ),
    ];

    let opened = live.ok("open_session", json!({"connection_id": ID}));
    assert_eq!(opened["supports_schemas"], false);
    let databases = live.ok("list_namespaces", json!({"connection_id": ID}));
    assert!(
        databases.to_string().contains(&format!("\"{database}\"")),
        "{databases}"
    );
    let schemas = live.ok(
        "list_namespaces",
        json!({"connection_id": ID, "database": database}),
    );
    assert_eq!(
        schemas["namespaces"],
        json!([]),
        "MySQL has no schema level"
    );

    for statement in [
        format!("CREATE TABLE {parent}(id int PRIMARY KEY, name varchar(50) NOT NULL COMMENT 'display name')"),
        format!("CREATE TABLE {child}(id int AUTO_INCREMENT PRIMARY KEY, parent_id int NOT NULL, note text, CONSTRAINT fk_parent FOREIGN KEY (parent_id) REFERENCES {parent}(id) ON DELETE CASCADE)"),
        format!("INSERT INTO {parent}(id, name) VALUES (1, 'one'), (2, 'two'), (3, 'three')"),
    ] {
        let outcome = live.sql(&statement, json!({"database": database}));
        assert_eq!(code(&outcome), "ok", "{statement}: {outcome:?}");
    }
    let ns = json!({"connection_id": ID, "database": database});
    let mut listing = ns.clone();
    listing["filter"] = json!("tablex_agent");
    let tables = live.ok("list_tables", listing);
    let names: Vec<&str> = tables["tables"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&parent.as_str()) && names.contains(&child.as_str()),
        "{names:?}"
    );
    let mut describe = ns.clone();
    describe["table"] = json!(child);
    let child_description = live.ok("describe_table", describe);
    assert_eq!(child_description["primary_key"], json!(["id"]));
    assert_eq!(
        child_description["foreign_keys"][0]["referenced_table"],
        parent.as_str()
    );
    let mut describe = ns.clone();
    describe["table"] = json!(parent);
    assert_eq!(
        live.ok("describe_table", describe)["columns"][1]["comment"],
        "display name"
    );
    let mut context = ns.clone();
    context["budget"] = json!(200_000);
    let digest = live.ok("get_schema_context", context.clone());
    let text = digest["text"].as_str().unwrap();
    assert!(
        // MariaDB reports `int(11)`, MySQL 8 `int`.
        text.contains(&format!("TABLE {parent}\n  id int"))
            && text.contains(" NOT NULL PK\n  name "),
        "{text}"
    );
    assert!(text.contains("-- display name"), "{text}");
    assert!(text.contains("ON DELETE CASCADE"), "{text}");
    assert_eq!(
        live.ok("get_schema_context", context),
        digest,
        "deterministic"
    );

    let rows = live.sql(
        &format!("SELECT id, name FROM {parent} ORDER BY id"),
        json!({"database": database}),
    );
    match &rows {
        ToolOutcome::Success(value) => assert_eq!(
            value["rows"],
            json!([["1", "one"], ["2", "two"], ["3", "three"]])
        ),
        ToolOutcome::Failure(error) => panic!("{error:?}"),
    }
    let plan = live.ok("explain_query", json!({"connection_id": ID, "database": database, "statement": format!("SELECT * FROM {parent} WHERE id = 2")}));
    assert!(plan["row_count"].as_u64().unwrap() >= 1, "{plan}");
    live.catalog
        .update(ID, |info| info.statement_timeout_secs = Some(1));
    let slow = live.sql("SELECT SLEEP(30)", json!({"database": database}));
    assert_eq!(code(&slow), "agent.limit", "{slow:?}");
    live.catalog
        .update(ID, |info| info.statement_timeout_secs = None);
    assert_eq!(
        code(&live.sql("SELECT 1", json!({"database": database}))),
        "ok"
    );

    let made = live.sql(
        &format!("CREATE TABLE {policy}(id int PRIMARY KEY, name varchar(20))"),
        json!({"database": database}),
    );
    assert_eq!(code(&made), "ok", "{made:?}");
    let seeded = live.sql(
        &format!("INSERT INTO {policy}(id, name) VALUES (1, 'a'), (2, 'b')"),
        json!({"database": database}),
    );
    assert_eq!(code(&seeded), "ok", "{seeded:?}");
    sql_policy_matrix(&live, &policy, Some(&database));
    assert_eq!(
        code(&live.sql(
            &format!("DROP TABLE {policy}"),
            json!({"database": database})
        )),
        "agent.dangerous_refused"
    );
    assert_eq!(
        code(&live.sql(
            &format!("DROP TABLE {policy}"),
            json!({"database": database, "dangerous": true})
        )),
        "ok"
    );
    assert_eq!(
        code(&live.sql(
            &format!("DROP TABLE {child}"),
            json!({"database": database, "dangerous": true})
        )),
        "ok"
    );
    assert_eq!(
        code(&live.sql(
            &format!("DROP TABLE {parent}"),
            json!({"database": database, "dangerous": true})
        )),
        "ok"
    );
}

#[test]
#[ignore = "needs TABLEX_TEST_CLICKHOUSE_URI"]
fn clickhouse_tools_and_policy() {
    let Ok(uri) = std::env::var("TABLEX_TEST_CLICKHOUSE_URI") else {
        return;
    };
    let database = database_of(&uri);
    let mut live = Live::new(
        ConnectionKind::ClickHouse,
        "dbengine-clickhouse",
        &uri,
        AgentAccess::ReadWrite,
    );
    let table = unique("tablex_agent_ch");
    live.cleanup = vec![(format!("DROP TABLE IF EXISTS {database}.{table}"), None)];
    let ns = json!({"connection_id": ID, "database": database});

    live.ok("open_session", json!({"connection_id": ID}));
    let databases = live.ok("list_namespaces", json!({"connection_id": ID}));
    assert!(
        databases.to_string().contains(&format!("\"{database}\"")),
        "{databases}"
    );
    let made = live.sql(&format!("CREATE TABLE {database}.{table} (id UInt32, name String COMMENT 'display name') ENGINE = Memory"), json!({}));
    assert_eq!(code(&made), "ok", "{made:?}");
    let seeded = live.sql(
        &format!("INSERT INTO {database}.{table} VALUES (1, 'one'), (2, 'two')"),
        json!({}),
    );
    assert_eq!(code(&seeded), "ok", "{seeded:?}");
    let mut listing = ns.clone();
    listing["filter"] = json!("tablex_agent_ch");
    assert!(live.ok("list_tables", listing).to_string().contains(&table));
    let mut describe = ns.clone();
    describe["table"] = json!(table);
    let described = live.ok("describe_table", describe);
    assert_eq!(described["columns"][1]["comment"], "display name");
    let rows = live.sql(
        &format!("SELECT id, name FROM {database}.{table} ORDER BY id"),
        json!({}),
    );
    match &rows {
        ToolOutcome::Success(value) => {
            assert_eq!(value["rows"], json!([["1", "one"], ["2", "two"]]));
            assert_eq!(value["read_only_enforced"], true);
        }
        ToolOutcome::Failure(error) => panic!("{error:?}"),
    }
    // A write after a read-only read still works: the setting is per query, not per session.
    let more = live.sql(
        &format!("INSERT INTO {database}.{table} VALUES (3, 'three')"),
        json!({}),
    );
    assert_eq!(code(&more), "ok", "{more:?}");

    // Layer 1: table functions and file clauses, at every level.
    for statement in [
        "SELECT * FROM url('http://169.254.169.254/latest/meta-data/', CSV)",
        "SELECT * FROM file('/etc/passwd', 'LineAsString')",
        "SELECT * FROM remote('127.0.0.1', system.one)",
        "SELECT * FROM s3('https://example.invalid/x.csv')",
        "SELECT * FROM cluster('default', system.one)",
        "SELECT 1 INTO OUTFILE '/tmp/x'",
    ] {
        for access in [AgentAccess::Read, AgentAccess::ReadWrite] {
            live.catalog
                .update(ID, |info| info.agent_access = Some(access));
            let outcome = live.sql(statement, json!({}));
            assert_eq!(
                code(&outcome),
                "agent.blocked_function",
                "{access}: {statement}: {outcome:?}"
            );
        }
    }
    // Layer 2: with the denylist bypassed, `readonly` stops writes (and table functions) in the server.
    let insert = live
        .raw_run(
            &format!("INSERT INTO {database}.{table} VALUES (9, 'x')"),
            None,
            true,
        )
        .unwrap_err();
    assert!(
        matches!(insert.code(), "agent.write_refused" | "query.failed"),
        "{insert:?}"
    );
    assert!(
        insert.to_string().to_lowercase().contains("readonly")
            || insert.code() == "agent.write_refused",
        "{insert}"
    );
    // `readonly` does not stop ClickHouse table functions (`url(...)` still runs, and
    // can hang), which is why layer 1 refuses them by name.
    let count = live
        .raw_run(
            &format!("SELECT count() FROM {database}.{table} WHERE id = 9"),
            None,
            true,
        )
        .unwrap();
    assert_eq!(count.rows[0][0], "0");
    assert!(count.read_only_enforced);
    // The session is writable again afterwards.
    live.raw_run(
        &format!("INSERT INTO {database}.{table} VALUES (10, 'ten')"),
        None,
        false,
    )
    .unwrap();
    live.catalog
        .update(ID, |info| info.agent_access = Some(AgentAccess::ReadWrite));
    assert_eq!(
        code(&live.sql("USE system", json!({}))),
        "agent.unsupported"
    );
    assert_eq!(
        code(&live.sql("SET max_threads = 1", json!({}))),
        "agent.unsupported"
    );
    // Cleanup through the policy.
    assert_eq!(
        code(&live.sql(
            &format!("DROP TABLE {database}.{table}"),
            json!({"dangerous": true})
        )),
        "ok"
    );
}

/// The dedicated Redis database index of these tests.
const REDIS_DB: &str = "13";

#[test]
#[ignore = "needs TABLEX_TEST_REDIS_URI"]
fn redis_tools_and_policy() {
    let Ok(uri) = std::env::var("TABLEX_TEST_REDIS_URI") else {
        return;
    };
    let mut live = Live::new(
        ConnectionKind::Redis,
        "dbengine-redis",
        &uri,
        AgentAccess::ReadWrite,
    );
    let prefix = unique("tablex_agent");
    let (string_key, hash_key) = (format!("{prefix}:string"), format!("{prefix}:hash"));
    live.cleanup = vec![(
        format!("DEL {string_key} {hash_key}"),
        Some(REDIS_DB.to_owned()),
    )];
    let redis =
        |command: &str| json!({"connection_id": ID, "command": command, "database": REDIS_DB});

    let opened = live.ok("open_session", json!({"connection_id": ID}));
    assert_eq!(opened["language"], "redis");
    let databases = live.ok("list_namespaces", json!({"connection_id": ID}));
    assert!(
        databases["namespaces"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["name"] == REDIS_DB),
        "{databases}"
    );
    // SQL tools are not for Redis.
    assert_eq!(
        code(&live.call("list_tables", json!({"connection_id": ID}))),
        "agent.unsupported"
    );
    assert_eq!(code(&live.sql("SELECT 1", json!({}))), "agent.unsupported");

    // Read + write: an approved write runs (the approver is asked), reads need no question.
    let asked_before = live.approver.asked.lock().unwrap().len();
    let set = live.call(
        "execute_redis_command",
        redis(&format!("SET {string_key} hello")),
    );
    assert_eq!(code(&set), "ok", "{set:?}");
    assert_eq!(live.approver.asked.lock().unwrap().len(), asked_before + 1);
    let hset = live.call(
        "execute_redis_command",
        redis(&format!("HSET {hash_key} a 1 b 2")),
    );
    assert_eq!(code(&hset), "ok", "{hset:?}");
    let get = live.ok("execute_redis_command", redis(&format!("GET {string_key}")));
    assert!(get["rows"].to_string().contains("hello"), "{get}");
    let hash = live.ok(
        "execute_redis_command",
        redis(&format!("HGETALL {hash_key}")),
    );
    assert!(hash["rows"].to_string().contains('2'), "{hash}");
    let scan = live.ok(
        "execute_redis_command",
        redis(&format!("SCAN 0 MATCH {prefix}:* COUNT 1000")),
    );
    assert!(scan["rows"].to_string().contains(&prefix), "{scan}");
    assert_eq!(
        live.approver.asked.lock().unwrap().len(),
        asked_before + 2,
        "reads never ask"
    );

    // Policy: blocked, dangerous and multi-line commands never reach the server.
    for blocked in [
        "KEYS *",
        "CONFIG GET *",
        "EVAL \"return 1\" 0",
        "FLUSHALL",
        "DEBUG SLEEP 0",
    ] {
        let outcome = live.call("execute_redis_command", redis(blocked));
        assert!(
            matches!(code(&outcome), "agent.dangerous_refused"),
            "{blocked}: {outcome:?}"
        );
    }
    let flush = live.call("execute_redis_command", redis("FLUSHDB"));
    assert_eq!(code(&flush), "agent.dangerous_refused");
    assert_eq!(
        code(&live.call("execute_redis_command", redis("GET a\nGET b"))),
        "agent.multi_statement"
    );
    // The key is still there after the refused FLUSHDB.
    let still = live.ok("execute_redis_command", redis(&format!("GET {string_key}")));
    assert!(still["rows"].to_string().contains("hello"));

    // Read: writes are refused.
    live.catalog
        .update(ID, |info| info.agent_access = Some(AgentAccess::Read));
    for write in [
        format!("SET {string_key} changed"),
        format!("DEL {string_key}"),
        format!("PUBLISH {prefix} x"),
        format!("EXPIRE {string_key} 1"),
    ] {
        assert_eq!(
            code(&live.call("execute_redis_command", redis(&write))),
            "agent.write_refused",
            "{write}"
        );
    }
    let unchanged = live.ok("execute_redis_command", redis(&format!("GET {string_key}")));
    assert!(unchanged["rows"].to_string().contains("hello"));
    live.catalog
        .update(ID, |info| info.agent_access = Some(AgentAccess::SchemaOnly));
    assert_eq!(
        code(&live.call("execute_redis_command", redis("PING"))),
        "agent.access_denied"
    );
    live.catalog
        .update(ID, |info| info.agent_access = Some(AgentAccess::ReadWrite));
    // Cleanup through the policy: a delete is an ordinary approved write.
    assert_eq!(
        code(&live.call(
            "execute_redis_command",
            redis(&format!("DEL {string_key} {hash_key}"))
        )),
        "ok"
    );
}

#[test]
#[ignore = "needs TABLEX_TEST_MONGO_URI"]
fn mongodb_read_only_tools() {
    let Ok(uri) = std::env::var("TABLEX_TEST_MONGO_URI") else {
        return;
    };
    let live = Live::new(ConnectionKind::MongoDb, "dbengine-mongo", &uri, AgentAccess::Read);
    // `admin.system.version` holds the server's featureCompatibilityVersion document: no data of
    // ours is needed, and nothing is written.
    let target = |extra: Value| {
        let mut args = json!({"connection_id": ID, "database": "admin", "collection": "system.version"});
        for (key, value) in extra.as_object().into_iter().flatten() {
            args[key] = value.clone();
        }
        args
    };
    let opened = live.ok("open_session", json!({"connection_id": ID}));
    assert_eq!(opened["language"], "mongo");
    let databases = live.ok("list_namespaces", json!({"connection_id": ID}));
    assert!(
        databases["namespaces"].as_array().unwrap().iter().any(|d| d["name"] == "admin"),
        "{databases}"
    );

    let counted = live.ok("mongo_count", target(json!({})));
    assert!(counted["count"].as_u64().unwrap() >= 1, "{counted}");
    let found = live.ok(
        "mongo_find",
        target(json!({"filter": {"_id": "featureCompatibilityVersion"}, "projection": {"version": 1}})),
    );
    assert_eq!(found["document_count"], 1, "{found}");
    assert_eq!(found["truncated"], false);
    assert_eq!(found["documents"][0]["_id"], "featureCompatibilityVersion");
    assert!(found["documents"][0]["version"].is_string(), "{found}");
    // A typed value in the filter (Extended JSON) reaches the server as a date.
    let typed = live.ok(
        "mongo_count",
        target(json!({"filter": {
            "_id": "featureCompatibilityVersion",
            "$expr": {"$lt": [{"$toDate": {"$numberLong": "0"}}, {"$date": "2030-01-01T00:00:00Z"}]},
        }})),
    );
    assert_eq!(typed["count"], 1);
    let none = live.ok(
        "mongo_find",
        target(json!({"filter": {"_id": {"$oid": "65b9f0a1c2d3e4f506172839"}}})),
    );
    assert_eq!(none["document_count"], 0);
    assert_eq!(none["truncated"], false);

    // 2500 documents the server makes up: the limits cut them, as relaxed Extended JSON.
    let numbers = json!([
        {"$limit": 1},
        {"$project": {"_id": 0, "n": {"$range": [0, 2500]}}},
        {"$unwind": "$n"},
    ]);
    let few = live.ok("mongo_aggregate", target(json!({"pipeline": numbers, "limit": 5})));
    assert_eq!(few["documents"], json!([{"n": 0}, {"n": 1}, {"n": 2}, {"n": 3}, {"n": 4}]));
    assert_eq!(few["truncated"], true);
    assert_eq!(few["truncation"], "row_limit");
    let default = live.ok("mongo_aggregate", target(json!({"pipeline": numbers})));
    assert_eq!(default["document_count"], 200);
    assert_eq!(default["limit"], 200);
    assert_eq!(default["truncated"], true);
    let capped = live.ok("mongo_aggregate", target(json!({"pipeline": numbers, "limit": 50000})));
    assert_eq!(capped["limit"], 1000);
    assert!(capped["document_count"].as_u64().unwrap() <= 1000);
    assert_eq!(capped["truncated"], true);
    let exact = live.ok("mongo_aggregate", target(json!({"pipeline": [numbers[0], numbers[1], numbers[2], {"$match": {"n": {"$lt": 3}}}]})));
    assert_eq!(exact["document_count"], 3);
    assert_eq!(exact["truncated"], false);

    // The server's own refusal is an error with its name.
    let bad = live.call("mongo_find", target(json!({"filter": {"$nope": 1}})));
    assert_eq!(code(&bad), "query.failed", "{bad:?}");
    assert!(bad.to_text().contains("unknown top level operator"), "{}", bad.to_text());

    // Nothing writes: a pipeline that would is refused before it is sent.
    let scratch = unique("tablex_agent_out");
    for stage in [
        json!({"$out": scratch}),
        json!({"$merge": {"into": scratch}}),
    ] {
        let refused = live.call("mongo_aggregate", target(json!({"pipeline": [stage]})));
        assert_eq!(code(&refused), "agent.write_refused", "{refused:?}");
    }
    let collections = live.ok("list_tables", json!({"connection_id": ID, "database": "admin"}));
    assert!(
        !collections["tables"].as_array().unwrap().iter().any(|t| t["name"] == scratch.as_str()),
        "{collections}"
    );

    // The SQL tools point to the MongoDB ones.
    let sql = live.sql("SELECT 1", json!({}));
    assert_eq!(code(&sql), "agent.unsupported");
    assert!(sql.to_text().contains("mongo_find"), "{}", sql.to_text());

    // A command the server is still working on stops when the call is cancelled.
    let token = CancelToken::new();
    let stopper = {
        let token = token.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(500));
            token.cancel();
        })
    };
    let started = std::time::Instant::now();
    let slow = live.layer.call_cancellable(
        "mongo_count",
        target(json!({"filter": {"$where": "sleep(20000) || true"}})),
        &token,
    );
    stopper.join().unwrap();
    assert_eq!(code(&slow), "agent.cancelled", "{slow:?}");
    assert!(started.elapsed() < std::time::Duration::from_secs(15), "{:?}", started.elapsed());
    // The session still works.
    live.ok("mongo_count", target(json!({})));

    // Below `read` the tools are refused.
    live.catalog
        .update(ID, |info| info.agent_access = Some(AgentAccess::SchemaOnly));
    assert_eq!(code(&live.call("mongo_count", target(json!({})))), "agent.access_denied");
}

/// Drops a scratch MongoDB database outside the tool layer when it goes out of scope.
struct DropMongoDatabase {
    uri: String,
    database: String,
}

impl Drop for DropMongoDatabase {
    fn drop(&mut self) {
        let handle = load_driver("dbengine-mongo", "abi-v2");
        let Ok(mut session) = handle.connect(&json!({"uri": self.uri}).to_string()) else {
            return;
        };
        let _ = session.mutate(&engine_driver_api::MutationRequest {
            operation_id: engine_driver_api::OperationId(1),
            kind: "drop_database".into(),
            database: Some(self.database.clone()),
            object: None,
            identity: Value::Null,
            payload: json!({}),
        });
    }
}

#[test]
#[ignore = "needs TABLEX_TEST_MONGO_URI"]
fn mongodb_writes_in_a_throwaway_database() {
    let Ok(uri) = std::env::var("TABLEX_TEST_MONGO_URI") else {
        return;
    };
    let database = unique("tablex_agent_writes");
    let _drop = DropMongoDatabase {
        uri: uri.clone(),
        database: database.clone(),
    };
    let live = Live::new(ConnectionKind::MongoDb, "dbengine-mongo", &uri, AgentAccess::ReadWrite);
    let target = |extra: Value| {
        let mut args = json!({"connection_id": ID, "database": database, "collection": "orders"});
        for (key, value) in extra.as_object().into_iter().flatten() {
            args[key] = value.clone();
        }
        args
    };
    live.ok("open_session", json!({"connection_id": ID}));
    let count = |filter: Value| live.ok("mongo_count", target(json!({"filter": filter})))["count"].as_u64().unwrap();

    // Insert, with typed values.
    let inserted = live.ok(
        "mongo_insert",
        target(json!({"documents": [
            {"_id": 1, "status": "open", "total": 10, "placed": {"$date": "2024-01-31T00:00:00Z"}},
            {"_id": 2, "status": "open", "total": 20},
            {"_id": 3, "status": "paid", "total": 30},
        ]})),
    );
    assert_eq!(inserted["n"], 3, "{inserted}");
    assert!(inserted.get("write_errors").is_none());
    assert_eq!(count(json!({})), 3);
    let placed = live.ok("mongo_find", target(json!({"filter": {"_id": 1}})));
    assert_eq!(placed["documents"][0]["placed"], json!({"$date": "2024-01-31T00:00:00Z"}));
    // The question showed the exact command, the database and the collection.
    let asked = live.approver.asked.lock().unwrap().clone();
    assert!(asked[0].0.contains("\"insert\": \"orders\""), "{}", asked[0].0);
    assert_eq!(live.approver.databases.lock().unwrap()[0].as_deref(), Some(database.as_str()));

    // A duplicate key is a write error in the reply, not a failed call.
    let duplicate = live.ok("mongo_insert", target(json!({"documents": [{"_id": 1}]})));
    assert_eq!(duplicate["n"], 0);
    assert_eq!(duplicate["write_errors"][0]["code"], 11000, "{duplicate}");

    // Update: one match by default, every match with `many`, a pipeline, an upsert.
    let one = live.ok(
        "mongo_update",
        target(json!({"filter": {"status": "open"}, "update": {"$set": {"status": "paid"}}})),
    );
    assert_eq!((one["n"].as_u64(), one["n_modified"].as_u64()), (Some(1), Some(1)), "{one}");
    assert_eq!(count(json!({"status": "paid"})), 2);
    let many = live.ok(
        "mongo_update",
        target(json!({
            "filter": {"status": "paid"},
            "pipeline": [{"$set": {"total": {"$add": ["$total", 1]}}}],
            "many": true,
        })),
    );
    assert_eq!((many["n"].as_u64(), many["n_modified"].as_u64()), (Some(2), Some(2)), "{many}");
    let totals = live.ok("mongo_find", target(json!({"filter": {"status": "paid"}, "sort": {"_id": 1}, "projection": {"total": 1}})));
    let totals: Vec<i64> = totals["documents"].as_array().unwrap().iter().map(|d| d["total"].as_i64().unwrap()).collect();
    assert_eq!(totals.len(), 2);
    assert!(totals.iter().all(|total| [11, 21, 31].contains(total)), "{totals:?}");
    let upserted = live.ok(
        "mongo_update",
        target(json!({"filter": {"_id": 99}, "update": {"$set": {"status": "new"}}, "upsert": true})),
    );
    assert_eq!(upserted["n"], 1);
    assert_eq!(upserted["n_modified"], 0);
    assert_eq!(upserted["upserted"][0]["_id"], 99, "{upserted}");
    assert_eq!(count(json!({})), 4);
    // Nothing matches and nothing is upserted.
    let none = live.ok("mongo_update", target(json!({"filter": {"_id": 1000}, "update": {"$set": {"a": 1}}})));
    assert_eq!((none["n"].as_u64(), none["n_modified"].as_u64()), (Some(0), Some(0)));
    // The server rejects a malformed update as a write error of the reply; nothing changed.
    let bad = live.ok("mongo_update", target(json!({"filter": {"_id": 1}, "update": {"$bogus": {"a": 1}}})));
    assert_eq!(bad["n"], 0);
    assert_eq!(bad["write_errors"][0]["code"], 9, "{bad}");
    assert_eq!(count(json!({})), 4);

    // An empty filter is refused unless `allow_all`; then the user is asked as for a dangerous change.
    let refused = live.call("mongo_delete", target(json!({"filter": {}, "many": true})));
    assert_eq!(code(&refused), "agent.invalid_arguments");
    assert_eq!(count(json!({})), 4);
    let deleted = live.ok("mongo_delete", target(json!({"filter": {"status": "new"}})));
    assert_eq!(deleted["n"], 1);
    let first = live.ok("mongo_delete", target(json!({"filter": {"status": "paid"}})));
    assert_eq!(first["n"], 1, "only the first match without `many`");
    assert_eq!(count(json!({})), 2);
    let all = live.ok("mongo_delete", target(json!({"filter": {}, "many": true, "allow_all": true})));
    assert_eq!(all["n"], 2);
    assert_eq!(count(json!({})), 0);
    let dangerous = live.approver.asked.lock().unwrap().iter().filter(|(_, danger)| danger.is_some()).count();
    assert_eq!(dangerous, 1);

    // Below `read_write` the tools are refused and nothing is sent.
    live.catalog
        .update(ID, |info| info.agent_access = Some(AgentAccess::Read));
    for (tool, args) in [
        ("mongo_insert", target(json!({"documents": [{"_id": 7}]}))),
        ("mongo_update", target(json!({"filter": {"_id": 7}, "update": {"$set": {"a": 1}}}))),
        ("mongo_delete", target(json!({"filter": {"_id": 7}}))),
    ] {
        assert_eq!(code(&live.call(tool, args)), "agent.access_denied", "{tool}");
    }
    assert_eq!(count(json!({})), 0);
}

fn tx_sql(live: &Live, tx: &str, statement: &str) -> ToolOutcome {
    live.sql(statement, json!({"transaction_id": tx}))
}

fn first_cell(outcome: &ToolOutcome) -> String {
    match outcome {
        ToolOutcome::Success(value) => value["rows"][0][0].as_str().unwrap().to_owned(),
        ToolOutcome::Failure(error) => panic!("{error:?}"),
    }
}

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI"]
fn postgresql_transactions() {
    let Ok(uri) = std::env::var("TABLEX_TEST_PG_URI") else {
        return;
    };
    let mut live = Live::new(
        ConnectionKind::PostgreSql,
        "dbengine-postgres",
        &uri,
        AgentAccess::ReadWrite,
    );
    let table = unique("tablex_agent_tx");
    live.cleanup = vec![(format!("DROP TABLE IF EXISTS {table}"), None)];
    live.ok("open_session", json!({"connection_id": ID}));
    let created = live.sql(&format!("CREATE TABLE {table}(id integer PRIMARY KEY, v text)"), json!({}));
    assert_eq!(code(&created), "ok", "{created:?}");
    let begin = |live: &Live| {
        live.ok("begin_transaction", json!({"connection_id": ID}))["transaction_id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let shared_count = |live: &Live| {
        first_cell(&live.sql(&format!("SELECT count(*) FROM {table}"), json!({})))
    };

    // Rolled back: the write is visible inside (to a read-only read, in a savepoint) and nowhere else.
    let tx = begin(&live);
    let inserted = tx_sql(&live, &tx, &format!("INSERT INTO {table} VALUES (1, 'a')"));
    assert_eq!(code(&inserted), "ok", "{inserted:?}");
    let inside = tx_sql(&live, &tx, &format!("SELECT count(*) FROM {table}"));
    assert_eq!(first_cell(&inside), "1");
    assert!(matches!(&inside, ToolOutcome::Success(v) if v["read_only_enforced"] == true));
    // After a read-only read the transaction is still writable (the savepoint scope was left).
    let more = tx_sql(&live, &tx, &format!("INSERT INTO {table} VALUES (2, 'b')"));
    assert_eq!(code(&more), "ok", "{more:?}");
    // Both writes are still there after the read-only read in its savepoint was left.
    assert_eq!(first_cell(&tx_sql(&live, &tx, &format!("SELECT count(*) FROM {table}"))), "2");
    // COPY (files, programs) is not undone by a rollback: refused in a transaction.
    assert_eq!(
        code(&tx_sql(&live, &tx, &format!("COPY {table} TO '/tmp/tablex_agent_copy.csv'"))),
        "agent.unsupported"
    );
    assert_eq!(shared_count(&live), "0", "an uncommitted row is invisible to another session");
    let rolled = live.ok("rollback_transaction", json!({"transaction_id": tx}));
    assert_eq!(rolled["changes"], 2);
    assert_eq!(shared_count(&live), "0");

    // Committed: the rows stay, and the commit was asked with both writes.
    let tx = begin(&live);
    tx_sql(&live, &tx, &format!("INSERT INTO {table} VALUES (3, 'c')"));
    let committed = live.ok("commit_transaction", json!({"transaction_id": tx}));
    assert_eq!(committed["committed"], true);
    assert_eq!(shared_count(&live), "1");

    // A failed statement aborts the transaction: everything but rollback is refused.
    let tx = begin(&live);
    let failed = tx_sql(&live, &tx, &format!("INSERT INTO {table} VALUES (3, 'duplicate key')"));
    assert!(failed.is_error());
    assert_eq!(code(&tx_sql(&live, &tx, "SELECT 1")), "agent.transaction_aborted");
    assert_eq!(
        code(&live.call("commit_transaction", json!({"transaction_id": tx}))),
        "agent.transaction_aborted"
    );
    live.ok("rollback_transaction", json!({"transaction_id": tx}));
    assert_eq!(shared_count(&live), "1");

    // A failing READ aborts the transaction in the layer's eyes too (a savepoint may have saved it
    // on the server, but a cancel or timeout cannot be told apart): a committed-after-error
    // transaction never silently becomes a rollback.
    let tx = begin(&live);
    tx_sql(&live, &tx, &format!("INSERT INTO {table} VALUES (5, 'e')"));
    let failed_read = tx_sql(&live, &tx, "SELECT * FROM tablex_agent_no_such_table");
    assert!(failed_read.is_error());
    assert_eq!(code(&tx_sql(&live, &tx, "SELECT 1")), "agent.transaction_aborted");
    assert_eq!(
        code(&live.call("commit_transaction", json!({"transaction_id": tx}))),
        "agent.transaction_aborted"
    );
    live.ok("rollback_transaction", json!({"transaction_id": tx}));
    assert_eq!(shared_count(&live), "1");

    // Closing the connection's session rolls an open transaction back.
    let tx = begin(&live);
    tx_sql(&live, &tx, &format!("INSERT INTO {table} VALUES (4, 'd')"));
    live.ok("close_session", json!({"connection_id": ID}));
    live.ok("open_session", json!({"connection_id": ID}));
    assert_eq!(shared_count(&live), "1");
    assert_eq!(
        live.ok("rollback_transaction", json!({"transaction_id": tx}))["rolled_back"],
        false
    );
}
