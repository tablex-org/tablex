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
    provider: Arc<DriverProvider>,
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
            ToolLayer::new(catalog.clone(), provider.clone(), "live-test")
                .with_transactions()
                .with_pinned_sessions()
                .with_approver(approver.clone()),
        );
        Self {
            layer,
            catalog,
            approver,
            raw,
            provider,
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

    // SQL is not a MongoDB command (execute_query says what it takes), and the other SQL tools
    // point to the MongoDB ones.
    let sql = live.sql("SELECT 1", json!({}));
    assert_eq!(code(&sql), "agent.invalid_arguments");
    assert!(sql.to_text().contains("starts with `db.`"), "{}", sql.to_text());
    let batch = live.call("execute_batch", json!({"connection_id": ID, "script": "SELECT 1"}));
    assert_eq!(code(&batch), "agent.unsupported");
    assert!(batch.to_text().contains("mongo_find"), "{}", batch.to_text());

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

/// The documents of an `execute_query` result on MongoDB (one JSON text per row).
fn shell_documents(result: &Value) -> Vec<Value> {
    result["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| serde_json::from_str(row[0].as_str().unwrap()).unwrap())
        .collect()
}

#[test]
#[ignore = "needs TABLEX_TEST_MONGO_URI"]
fn mongodb_shell_commands_in_a_throwaway_database() {
    let Ok(uri) = std::env::var("TABLEX_TEST_MONGO_URI") else {
        return;
    };
    let database = unique("tablex_agent_shell");
    let _drop = DropMongoDatabase {
        uri: uri.clone(),
        database: database.clone(),
    };
    let live = Live::new(ConnectionKind::MongoDb, "dbengine-mongo", &uri, AgentAccess::ReadWrite);
    let run = |statement: &str, extra: Value| {
        let mut args = json!({"connection_id": ID, "database": database, "statement": statement});
        for (key, value) in extra.as_object().into_iter().flatten() {
            args[key] = value.clone();
        }
        live.call("execute_query", args)
    };
    let ok = |statement: &str| match run(statement, json!({})) {
        ToolOutcome::Success(value) => value,
        ToolOutcome::Failure(error) => panic!("{statement}: {error:?}"),
    };
    let danger = |statement: &str| match run(statement, json!({"dangerous": true})) {
        ToolOutcome::Success(value) => value,
        ToolOutcome::Failure(error) => panic!("{statement}: {error:?}"),
    };
    let names = |statement: &str| -> Vec<String> {
        let mut names: Vec<String> = shell_documents(&ok(statement))
            .iter()
            .map(|document| document["name"].as_str().unwrap().to_owned())
            .collect();
        names.sort();
        names
    };
    let opened = live.ok("open_session", json!({"connection_id": ID}));
    assert_eq!(opened["language"], "mongo");

    // Writes: insert (typed values); a duplicate key is a write error in the reply.
    let inserted = ok("db.orders.insertMany([{_id: 1, status: 'open', total: 10, placed: ISODate('2024-01-31T00:00:00Z')}, {_id: 2, status: 'open', total: 20}, {_id: 3, status: 'paid', total: 30}])");
    assert_eq!(inserted["affected_rows"], 3, "{inserted}");
    let duplicate = ok("db.orders.insertOne({_id: 1})");
    assert_eq!(shell_documents(&duplicate)[0]["writeErrors"][0]["code"], 11000, "{duplicate}");

    // Reads: find with a cursor chain, typed values, counts, distinct, a cut.
    let found = ok("db.orders.find({total: {$gte: 20}}, {total: 1}).sort({total: -1}).limit(5)");
    assert_eq!(shell_documents(&found), [json!({"_id": 3, "total": 30}), json!({"_id": 2, "total": 20})]);
    let typed = ok("db.getCollection('orders').findOne({placed: {$lt: ISODate('2025-01-01')}})");
    assert_eq!(shell_documents(&typed)[0]["_id"], 1);
    assert_eq!(ok("db.orders.countDocuments({status: 'open'})")["rows"], json!([["2"]]));
    assert_eq!(ok("db.orders.estimatedDocumentCount()")["rows"], json!([["3"]]));
    assert_eq!(ok("db.orders.find({}).count()")["rows"], json!([["3"]]));
    assert_eq!(shell_documents(&ok("db.orders.distinct('status')")).len(), 2);
    let cut = match run("db.orders.find({}).sort({_id: 1})", json!({"row_limit": 2})) {
        ToolOutcome::Success(value) => value,
        other => panic!("{other:?}"),
    };
    assert_eq!((cut["row_count"].as_u64(), cut["truncated"].as_bool()), (Some(2), Some(true)), "{cut}");
    let exact = ok("db.orders.find({}).limit(2)");
    assert_eq!((exact["row_count"].as_u64(), exact["truncated"].as_bool()), (Some(2), Some(false)), "{exact}");
    let aggregated = ok("db.orders.aggregate([{$group: {_id: '$status', n: {$sum: 1}}}, {$sort: {_id: 1}}])");
    assert_eq!(shell_documents(&aggregated), [json!({"_id": "open", "n": 2}), json!({"_id": "paid", "n": 1})]);

    // Indexes: the case that started it. createIndex, getIndexes, describe_table, the plan, the sizes.
    ok("db.orders.createIndex({status: 1, total: -1})");
    ok("db.orders.createIndex({total: 1}, {name: 'by_total', unique: true})");
    assert_eq!(names("db.getCollection('orders').getIndexes()"), ["_id_", "by_total", "status_1_total_-1"]);
    let described = live.ok("describe_table", json!({"connection_id": ID, "database": database, "table": "orders"}));
    let mut described_names: Vec<&str> = described["indexes"].as_array().unwrap().iter().map(|i| i["name"].as_str().unwrap()).collect();
    described_names.sort_unstable();
    assert_eq!(described_names, ["_id_", "by_total", "status_1_total_-1"], "{described}");
    assert_eq!(described["unique_columns"], json!(["total"]));
    assert_eq!(described["primary_key"], json!(["_id"]));
    assert!(described["ddl"].as_str().unwrap().contains("createIndex"), "{described}");
    let plan = shell_documents(&ok("db.orders.find({status: 'open'}).explain('queryPlanner')"));
    assert!(plan[0]["queryPlanner"].is_object(), "{plan:?}");
    assert!(shell_documents(&ok("db.orders.stats()"))[0]["count"].as_u64().is_some());
    assert!(shell_documents(&ok("db.orders.storageSize()"))[0]["storageSize"].as_u64().is_some());
    assert!(names("show collections").contains(&"orders".to_owned()));
    assert!(shell_documents(&ok("show dbs")).iter().any(|d| d["name"] == "admin"));
    assert!(shell_documents(&ok("db.version()"))[0]["version"].as_str().unwrap().contains('.'));
    let versions = ok("db.getSiblingDB('admin').getCollection('system.version').countDocuments({})");
    assert!(versions["rows"][0][0].as_str().unwrap().parse::<u64>().unwrap() >= 1, "{versions}");
    assert_eq!(ok("db.stats()")["row_count"], 1);

    // Updates, replace, find-and-modify, bulk write, deletes.
    let one = ok("db.orders.updateOne({status: 'open'}, {$set: {status: 'paid'}})");
    assert_eq!(shell_documents(&one)[0], json!({"n": 1, "nModified": 1}), "{one}");
    let many = ok("db.orders.updateMany({status: 'paid'}, [{$set: {total: {$add: ['$total', 1]}}}])");
    assert_eq!(shell_documents(&many)[0]["nModified"], 2, "{many}");
    ok("db.orders.replaceOne({_id: 3}, {status: 'closed', total: 5})");
    let after = ok("db.orders.findOneAndUpdate({_id: 3}, {$set: {total: 6}}, {returnDocument: 'after'})");
    assert_eq!(shell_documents(&after)[0]["value"], json!({"_id": 3, "status": "closed", "total": 6}), "{after}");
    let removed = ok("db.orders.findOneAndDelete({_id: 3})");
    assert_eq!(shell_documents(&removed)[0]["value"]["_id"], 3);
    let bulk = ok("db.orders.bulkWrite([{insertOne: {document: {_id: 10, total: 100}}}, {insertOne: {document: {_id: 11, total: 110}}}, {updateOne: {filter: {_id: 10}, update: {$set: {total: 101}}}}, {deleteOne: {filter: {_id: 11}}}])");
    assert_eq!(shell_documents(&bulk)[0], json!({"n": 4, "nModified": 1}), "{bulk}");
    assert_eq!(ok("db.orders.countDocuments({_id: {$gte: 10}})")["rows"], json!([["1"]]));
    assert_eq!(shell_documents(&ok("db.orders.deleteOne({_id: 10})"))[0]["n"], 1);

    // Dangerous: refused without the flag (nothing changes, nobody is asked), asked about with it.
    let asked_before = live.approver.asked.lock().unwrap().len();
    for statement in [
        "db.orders.deleteMany({})",
        "db.orders.dropIndex('by_total')",
        "db.orders.drop()",
        "db.orders.renameCollection('renamed')",
        "db.orders.aggregate([{$out: 'copy'}])",
    ] {
        assert_eq!(code(&run(statement, json!({}))), "agent.dangerous_refused", "{statement}");
    }
    assert_eq!(live.approver.asked.lock().unwrap().len(), asked_before, "nobody was asked");
    assert_eq!(ok("db.orders.countDocuments({})")["rows"], json!([["2"]]));
    danger("db.orders.aggregate([{$match: {}}, {$out: 'copy'}])");
    assert_eq!(ok("db.copy.countDocuments({})")["rows"], json!([["2"]]));
    danger("db.orders.dropIndex('by_total')");
    assert_eq!(names("db.orders.getIndexes()"), ["_id_", "status_1_total_-1"]);
    danger("db.copy.renameCollection('renamed')");
    assert_eq!(names("show collections"), ["orders", "renamed"]);
    danger("db.renamed.drop()");
    assert_eq!(names("show collections"), ["orders"]);
    let wiped = danger("db.orders.deleteMany({})");
    assert_eq!(shell_documents(&wiped)[0]["n"], 2);
    let asked = live.approver.asked.lock().unwrap().clone();
    assert_eq!(asked.iter().filter(|(_, danger)| danger.is_some()).count(), 5, "{asked:?}");

    // A schema digest from a sample, and a filter that matches every document is dangerous.
    ok("db.digest_probe.insertMany([{_id: 1, status: 'a', n: 1, owner: {name: 'x'}}, {_id: 2, status: 'b'}])");
    let digest = live.ok("get_schema_context", json!({"connection_id": ID, "database": database}));
    let text = digest["text"].as_str().unwrap();
    for part in ["digest_probe", "status", "owner.name", "int", "string"] {
        assert!(text.contains(part), "{part}: {text}");
    }
    let one = live.ok("get_schema_context", json!({"connection_id": ID, "database": database, "tables": ["digest_probe"], "max_tables": 1}));
    assert_eq!(one["tables_included"], 1, "{one}");
    for statement in ["db.digest_probe.deleteMany({_id: {$exists: true}})", "db.digest_probe.deleteMany({$expr: {$eq: [1, 1]}})", "db.digest_probe.updateMany({$or: [{n: 1}, {n: {$ne: 1}}]}, {$set: {n: 0}})"] {
        assert_eq!(code(&run(statement, json!({}))), "agent.dangerous_refused", "{statement}");
    }
    assert_eq!(ok("db.digest_probe.countDocuments({})")["rows"], json!([["2"]]));
    let wiped = danger("db.digest_probe.deleteMany({_id: {$exists: true}})");
    assert_eq!(shell_documents(&wiped)[0]["n"], 2, "{wiped}");

    // Never run: nothing reaches the server, whatever the flag says.
    for statement in ["db.runCommand({ping: 1})", "db.createUser({user: 'u', pwd: 'p', roles: []})"] {
        let refused = run(statement, json!({"dangerous": true}));
        assert_eq!(code(&refused), "agent.dangerous_refused", "{refused:?}");
        assert!(refused.to_text().contains("tablex app"), "{}", refused.to_text());
    }
    // The server's own error comes back with its name.
    let bad = run("db.orders.find({$nope: 1})", json!({}));
    assert_eq!(code(&bad), "query.failed", "{bad:?}");

    // Below `read_write` a change is refused and reads still run; below `read` nothing does,
    // except describe_table, which needs `schema_only`.
    live.catalog.update(ID, |info| info.agent_access = Some(AgentAccess::Read));
    assert_eq!(code(&run("db.orders.insertOne({_id: 7})", json!({}))), "agent.write_refused");
    assert_eq!(ok("db.orders.countDocuments({})")["rows"], json!([["0"]]));
    live.catalog.update(ID, |info| info.agent_access = Some(AgentAccess::SchemaOnly));
    assert_eq!(code(&run("db.orders.find({})", json!({}))), "agent.access_denied");
    live.ok("describe_table", json!({"connection_id": ID, "database": database, "table": "orders"}));
}

/// A user holding only the `read` role lists indexes and reads stats: no special role is needed.
/// Needs a mongod with authentication and two URIs: TABLEX_TEST_MONGO_URI (an administrator) and
/// TABLEX_TEST_MONGO_READER_URI (a user with the `read` role on the database
/// `tablex_reader_probe` and nothing else). The administrator creates the data, the reader reads.
#[test]
#[ignore = "needs TABLEX_TEST_MONGO_URI and TABLEX_TEST_MONGO_READER_URI"]
fn mongodb_shell_reads_with_a_read_only_user() {
    let (Ok(admin), Ok(reader)) = (
        std::env::var("TABLEX_TEST_MONGO_URI"),
        std::env::var("TABLEX_TEST_MONGO_READER_URI"),
    ) else {
        return;
    };
    let database = "tablex_reader_probe";
    let _drop = DropMongoDatabase {
        uri: admin.clone(),
        database: database.into(),
    };
    {
        let setup = Live::new(ConnectionKind::MongoDb, "dbengine-mongo", &admin, AgentAccess::ReadWrite);
        setup.ok("open_session", json!({"connection_id": ID}));
        for statement in [
            "db.orders.insertMany([{a: 1}, {a: 2}])",
            "db.orders.createIndex({a: 1}, {unique: true})",
        ] {
            let outcome = setup.call(
                "execute_query",
                json!({"connection_id": ID, "database": database, "statement": statement}),
            );
            assert_eq!(code(&outcome), "ok", "{statement}: {outcome:?}");
        }
    }
    let live = Live::new(ConnectionKind::MongoDb, "dbengine-mongo", &reader, AgentAccess::Read);
    live.ok("open_session", json!({"connection_id": ID}));
    let run = |statement: &str| {
        live.call(
            "execute_query",
            json!({"connection_id": ID, "database": database, "statement": statement}),
        )
    };
    for statement in [
        "db.getCollection('orders').getIndexes()",
        "db.orders.stats()",
        "db.orders.find({}).explain()",
        "db.orders.distinct('a')",
        "show collections",
        "db.orders.countDocuments({})",
    ] {
        let outcome = run(statement);
        assert_eq!(code(&outcome), "ok", "{statement}: {outcome:?}");
    }
    let described = live.ok("describe_table", json!({"connection_id": ID, "database": database, "table": "orders"}));
    assert_eq!(described["indexes"].as_array().unwrap().len(), 2, "{described}");
    // With the access raised, the server's role still stops a change.
    live.catalog.update(ID, |info| info.agent_access = Some(AgentAccess::ReadWrite));
    let write = run("db.orders.insertOne({a: 3})");
    assert!(
        matches!(write, ToolOutcome::Failure(_)) || write.to_text().contains("writeErrors"),
        "{write:?}"
    );
}

/// The value of the one cell of a result (`rows[0][0]`).
fn cell(outcome: &ToolOutcome) -> String {
    match outcome {
        ToolOutcome::Success(value) => value["rows"][0][0].as_str().unwrap_or("NULL").to_owned(),
        ToolOutcome::Failure(error) => panic!("{error:?}"),
    }
}

impl Live {
    /// Runs `statement` on the pinned session `session` (optionally in `database`).
    fn in_session(&self, session: &str, statement: &str, database: Option<&str>) -> ToolOutcome {
        let mut extra = json!({"session_id": session});
        if let Some(database) = database {
            extra["database"] = json!(database);
        }
        self.sql(statement, extra)
    }

    fn pin(&self) -> String {
        self.ok("open_session", json!({"connection_id": ID, "pinned": true}))["session_id"]
            .as_str()
            .unwrap()
            .to_owned()
    }
}

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI"]
fn postgresql_pinned_sessions() {
    let Ok(uri) = std::env::var("TABLEX_TEST_PG_URI") else {
        return;
    };
    let database = database_of(&uri);
    let schema = unique("tablex_agent_pin");
    let mut live = Live::new(ConnectionKind::PostgreSql, "dbengine-postgres", &uri, AgentAccess::ReadWrite);
    live.cleanup = vec![(format!("DROP SCHEMA IF EXISTS {schema} CASCADE"), None)];
    live.ok("open_session", json!({"connection_id": ID}));
    for statement in [
        format!("CREATE SCHEMA {schema}"),
        format!("CREATE TABLE {schema}.t(a integer)"),
        format!("INSERT INTO {schema}.t VALUES (1), (2)"),
    ] {
        let outcome = live.sql(&statement, json!({"database": database}));
        assert_eq!(code(&outcome), "ok", "{statement}: {outcome:?}");
    }
    let (a, b) = (live.pin(), live.pin());
    let on = |session: &str, statement: &str| live.in_session(session, statement, Some(&database));
    let default_schema = cell(&on(&b, "SELECT current_schema()"));
    assert_eq!(default_schema, "public");

    // SET persists in its session across calls, and is invisible to the others and to the shared one.
    assert_eq!(code(&on(&a, &format!("SET search_path TO {schema}, public"))), "ok");
    assert_eq!(cell(&on(&a, "SELECT current_schema()")), schema);
    assert_eq!(cell(&on(&a, "SELECT count(*) FROM t")), "2", "an unqualified name finds the table");
    assert_eq!(cell(&on(&b, "SELECT current_schema()")), "public");
    assert_ne!(code(&on(&b, "SELECT count(*) FROM t")), "ok", "the other session has no such table in its path");
    assert_eq!(cell(&live.sql("SELECT current_schema()", json!({"database": database}))), "public");
    // A time zone and a lock timeout too, each its own session's.
    assert_eq!(code(&on(&a, "SET TIME ZONE 'Asia/Tokyo'")), "ok");
    assert_eq!(code(&on(&a, "SET lock_timeout = '7s'")), "ok");
    assert_eq!(cell(&on(&a, "SHOW TimeZone")), "Asia/Tokyo");
    assert_eq!(cell(&on(&a, "SHOW lock_timeout")), "7s");
    assert_ne!(cell(&on(&b, "SHOW TimeZone")), "Asia/Tokyo");
    assert_eq!(cell(&on(&b, "SHOW lock_timeout")), "0");
    // RESET puts one back to what the session started with, not to another's value.
    assert_eq!(code(&on(&a, "RESET lock_timeout")), "ok");
    assert_eq!(cell(&on(&a, "SHOW lock_timeout")), "0");
    assert_eq!(code(&on(&a, "SET lock_timeout = '3s'")), "ok");
    assert_eq!(cell(&on(&a, "SHOW lock_timeout")), "3s");

    // Nothing that changes identity, privilege or the read-only mode reaches the server.
    for statement in [
        "SET ROLE postgres",
        "SET SESSION AUTHORIZATION postgres",
        "RESET ROLE",
        "SET default_transaction_read_only = off",
        "SET SESSION transaction_read_only = off",
        "SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE",
        "SET session_replication_role = replica",
        "RESET ALL",
        // PostgreSQL nests block comments: the server reads this as `SET ROLE postgres`.
        "SET /* /* */ search_path TO x /* */ */ ROLE postgres",
        "SET /* /* */ search_path TO x /* */ */ default_transaction_read_only = off",
    ] {
        let outcome = on(&a, statement);
        assert_eq!(code(&outcome), "agent.dangerous_refused", "{statement}: {outcome:?}");
    }
    assert_eq!(cell(&on(&a, "SELECT current_user = session_user")), "true");
    let hidden = on(&a, "SELECT set_config('default_transaction_read_only', 'off', false)");
    assert_eq!(code(&hidden), "agent.blocked_function", "{hidden:?}");
    // The session still has what it set (nothing was reset by the refusals).
    assert_eq!(cell(&on(&a, "SHOW lock_timeout")), "3s");

    // Reads stay read-only inside the session, whatever the session set; a write is approved.
    let read = on(&a, &format!("SELECT count(*) FROM {schema}.t"));
    assert_eq!(read.to_json()["read_only_enforced"], true, "{read:?}");
    let hidden_write = live.raw_run("SELECT 1", Some(&database), true);
    assert!(hidden_write.is_ok());
    live.catalog.update(ID, |info| info.agent_access = Some(AgentAccess::Read));
    let refused = on(&a, &format!("INSERT INTO {schema}.t VALUES (3)"));
    assert_eq!(code(&refused), "agent.write_refused", "{refused:?}");
    assert_eq!(code(&on(&a, "SET search_path TO public")), "ok", "a SET needs only `read`");
    assert_eq!(cell(&on(&a, &format!("SELECT count(*) FROM {schema}.t"))), "2");
    live.catalog.update(ID, |info| info.agent_access = Some(AgentAccess::ReadWrite));
    let asked = live.approver.asked.lock().unwrap().len();
    assert_eq!(code(&on(&a, &format!("INSERT INTO {schema}.t VALUES (3)"))), "ok");
    assert_eq!(live.approver.asked.lock().unwrap().len(), asked + 1);

    // A long cell is read in windows.
    let window = live.sql("SELECT repeat('0123456789', 500)", json!({"database": database, "cell_char_offset": 4990, "cell_char_limit": 20}));
    assert_eq!(cell(&window), "0123456789… [chars 4990-5000 of 5000]");
    let window = live.sql("SELECT repeat('0123456789', 500)", json!({"database": database, "cell_char_offset": 10, "cell_char_limit": 5}));
    assert_eq!(cell(&window), "01234… [chars 10-15 of 5000; next cell_char_offset 15]");

    // Closing drops the connection: a new session starts from the defaults, nothing leaks.
    live.ok("close_session", json!({"session_id": a}));
    let c = live.pin();
    assert_eq!(cell(&on(&c, "SHOW lock_timeout")), "0");
    assert_eq!(cell(&on(&c, "SHOW search_path")), "\"$user\", public");
    assert!(live.sql("SELECT 1", json!({"database": database, "session_id": a})).to_text().contains("unknown session"));
    // The audit of the session statements is the layer's; the server saw each session as a connection.
    let sessions = cell(&live.sql("SELECT count(*) FROM pg_stat_activity WHERE datname = current_database()", json!({"database": database})));
    assert!(sessions.parse::<u32>().unwrap() >= 3, "{sessions}");
}

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI"]
fn postgresql_lists_the_comment_of_each_table_with_the_table() {
    let Ok(uri) = std::env::var("TABLEX_TEST_PG_URI") else {
        return;
    };
    let database = database_of(&uri);
    let schema = unique("tablex_agent_comment");
    let mut live = Live::new(ConnectionKind::PostgreSql, "dbengine-postgres", &uri, AgentAccess::ReadWrite);
    live.cleanup = vec![(format!("DROP SCHEMA IF EXISTS {schema} CASCADE"), None)];
    live.ok("open_session", json!({"connection_id": ID}));
    for statement in [
        format!("CREATE SCHEMA {schema}"),
        format!("CREATE TABLE {schema}.commented(a integer)"),
        format!("CREATE TABLE {schema}.plain(a integer)"),
        format!("CREATE VIEW {schema}.v AS SELECT a FROM {schema}.commented"),
        format!("COMMENT ON TABLE {schema}.commented IS 'one row per order'"),
        format!("COMMENT ON VIEW {schema}.v IS 'the view'"),
    ] {
        let outcome = live.sql(&statement, json!({"database": database}));
        assert_eq!(code(&outcome), "ok", "{statement}: {outcome:?}");
    }
    let listed = live.ok("list_tables", json!({"connection_id": ID, "database": database, "schema": schema}));
    let tables = listed["tables"].as_array().unwrap();
    let comment = |name: &str| tables.iter().find(|t| t["name"] == name).unwrap_or_else(|| panic!("{name}: {listed}")).get("comment").cloned();
    assert_eq!(comment("commented"), Some(json!("one row per order")));
    assert_eq!(comment("v"), Some(json!("the view")));
    assert_eq!(comment("plain"), None);
}

#[test]
#[ignore = "needs TABLEX_TEST_MARIADB_URI"]
fn mariadb_pinned_sessions_transactions_and_comments() {
    mysql_family_pinned(ConnectionKind::MariaDb, "dbengine-mariadb", "TABLEX_TEST_MARIADB_URI");
}

#[test]
#[ignore = "needs TABLEX_TEST_MYSQL_URI"]
fn mysql_pinned_sessions_transactions_and_comments() {
    mysql_family_pinned(ConnectionKind::MySql, "dbengine-mysql", "TABLEX_TEST_MYSQL_URI");
}

fn mysql_family_pinned(kind: ConnectionKind, package: &'static str, env: &str) {
    let Ok(uri) = std::env::var(env) else {
        return;
    };
    let scratch = unique("tablex_agent_pin");
    let mut live = Live::new(kind, package, &uri, AgentAccess::ReadWrite);
    live.cleanup = vec![(format!("DROP DATABASE IF EXISTS {scratch}"), None)];
    live.ok("open_session", json!({"connection_id": ID}));
    for statement in [
        format!("CREATE DATABASE {scratch}"),
        format!("CREATE TABLE {scratch}.t(a int PRIMARY KEY, b varchar(20)) COMMENT 'one row per thing'"),
        format!("CREATE TABLE {scratch}.u(a int)"),
        format!("INSERT INTO {scratch}.t VALUES (1, 'x'), (2, 'y')"),
        format!("CREATE VIEW {scratch}.v AS SELECT a FROM {scratch}.t"),
    ] {
        let outcome = live.sql(&statement, json!({}));
        assert_eq!(code(&outcome), "ok", "{statement}: {outcome:?}");
    }

    // The comment of a table comes with the list, and a view has none.
    let listed = live.ok("list_tables", json!({"connection_id": ID, "database": scratch}));
    let tables = listed["tables"].as_array().unwrap();
    let find = |name: &str| tables.iter().find(|t| t["name"] == name).unwrap_or_else(|| panic!("{name}: {listed}"));
    assert_eq!(find("t")["comment"], "one row per thing");
    assert!(find("u").get("comment").is_none());
    assert!(find("v").get("comment").is_none(), "a view's column holds the word VIEW: {listed}");

    // USE and SET persist in their session only.
    let (a, b) = (live.pin(), live.pin());
    let on = |session: &str, statement: &str| live.in_session(session, statement, None);
    assert_eq!(code(&on(&a, &format!("USE {scratch}"))), "ok");
    assert_eq!(cell(&on(&a, "SELECT DATABASE()")), scratch);
    assert_eq!(cell(&on(&a, "SELECT count(*) FROM t")), "2", "an unqualified name finds the table");
    assert_ne!(cell(&on(&b, "SELECT DATABASE()")), scratch);
    assert_ne!(code(&on(&b, "SELECT count(*) FROM t")), "ok");
    assert_eq!(code(&on(&a, "SET @n = 41")), "ok");
    assert_eq!(cell(&on(&a, "SELECT @n + 1")), "42");
    assert_eq!(cell(&on(&b, "SELECT @n")), "NULL");
    assert_eq!(cell(&live.sql("SELECT @n", json!({}))), "NULL");
    assert_eq!(code(&on(&a, "SET time_zone = '+05:00'")), "ok");
    assert_eq!(cell(&on(&a, "SELECT @@session.time_zone")), "+05:00");
    assert_ne!(cell(&on(&b, "SELECT @@session.time_zone")), "+05:00");
    assert_eq!(code(&on(&a, "SET lock_wait_timeout = 9")), "ok");
    assert_eq!(cell(&on(&a, "SELECT @@session.lock_wait_timeout")), "9");
    assert_eq!(cell(&on(&a, "SELECT @@global.lock_wait_timeout")), cell(&on(&b, "SELECT @@session.lock_wait_timeout")));
    // The SET is not the server's own change: GLOBAL, PERSIST and the rest are refused before the server.
    for statement in [
        "SET GLOBAL lock_wait_timeout = 1",
        "SET @@global.lock_wait_timeout = 1",
        "SET SESSION TRANSACTION READ WRITE",
        "SET SESSION TRANSACTION READ ONLY",
        "SET sql_log_bin = 0",
        "SET sql_mode = ''",
        "SET autocommit = 0",
        "SET ROLE admin",
        "SET PASSWORD = 'x'",
        "SET foreign_key_checks = 0",
        "SET @@session.transaction_read_only = 0",
    ] {
        let outcome = on(&a, statement);
        assert_eq!(code(&outcome), "agent.dangerous_refused", "{statement}: {outcome:?}");
    }
    assert_eq!(cell(&on(&a, "SELECT @@global.lock_wait_timeout")), cell(&on(&b, "SELECT @@session.lock_wait_timeout")), "the server's own value is unchanged");
    assert_eq!(cell(&on(&a, "SELECT @n")), "41", "the refusals did not disturb the session");

    // Reads stay read-only in the session; a write is approved; at `read` it is refused.
    let read = on(&a, "SELECT count(*) FROM t");
    assert_eq!(read.to_json()["read_only_enforced"], true, "{read:?}");
    live.catalog.update(ID, |info| info.agent_access = Some(AgentAccess::Read));
    assert_eq!(code(&on(&a, "INSERT INTO t VALUES (3, 'z')")), "agent.write_refused");
    assert_eq!(code(&on(&a, "SET @m = 1")), "ok");
    live.catalog.update(ID, |info| info.agent_access = Some(AgentAccess::ReadWrite));
    assert_eq!(code(&on(&a, "INSERT INTO t VALUES (3, 'z')")), "ok");
    assert_eq!(cell(&live.sql(&format!("SELECT count(*) FROM {scratch}.t"), json!({}))), "3");

    // Closing drops the connection: the next session has nothing of it.
    live.ok("close_session", json!({"session_id": a}));
    let c = live.pin();
    assert_eq!(cell(&on(&c, "SELECT @n")), "NULL");
    assert_ne!(cell(&on(&c, "SELECT DATABASE()")), scratch);

    // Transactions: begin, a change that only the transaction sees, the commit question, a rollback.
    let tx = live.ok("begin_transaction", json!({"connection_id": ID}))["transaction_id"].as_str().unwrap().to_owned();
    let in_tx = |statement: &str| live.sql(statement, json!({"transaction_id": tx}));
    assert_eq!(code(&in_tx(&format!("INSERT INTO {scratch}.t VALUES (10, 'tx')"))), "ok");
    assert_eq!(cell(&in_tx(&format!("SELECT count(*) FROM {scratch}.t"))), "4", "the transaction sees its own change");
    assert_eq!(cell(&live.sql(&format!("SELECT count(*) FROM {scratch}.t"), json!({}))), "3", "nobody else does");
    let ddl = in_tx(&format!("CREATE TABLE {scratch}.w(a int)"));
    assert_eq!(code(&ddl), "agent.unsupported", "{ddl:?}");
    assert!(ddl.to_text().contains("COMMIT"), "{}", ddl.to_text());
    let asked = live.approver.asked.lock().unwrap().len();
    let committed = live.ok("commit_transaction", json!({"transaction_id": tx}));
    assert_eq!(committed["committed"], true, "{committed}");
    assert_eq!(live.approver.asked.lock().unwrap().len(), asked + 1, "the commit was put to the user");
    assert_eq!(cell(&live.sql(&format!("SELECT count(*) FROM {scratch}.t"), json!({}))), "4");
    let tx = live.ok("begin_transaction", json!({"connection_id": ID}))["transaction_id"].as_str().unwrap().to_owned();
    assert_eq!(code(&live.sql(&format!("DELETE FROM {scratch}.t WHERE a = 10"), json!({"transaction_id": tx}))), "ok");
    assert_eq!(cell(&live.sql(&format!("SELECT count(*) FROM {scratch}.t"), json!({"transaction_id": tx}))), "3");
    let rolled = live.ok("rollback_transaction", json!({"transaction_id": tx}));
    assert_eq!(rolled["rolled_back"], true);
    assert_eq!(cell(&live.sql(&format!("SELECT count(*) FROM {scratch}.t"), json!({}))), "4", "the rollback undid the delete");
    // A script as one transaction: committed, and rolled back on an error.
    let script = live.ok("execute_batch", json!({"connection_id": ID, "use_transaction": true, "script": format!("INSERT INTO {scratch}.t VALUES (20, 'a'); INSERT INTO {scratch}.t VALUES (21, 'b')")}));
    assert_eq!(script["transaction"], "committed", "{script}");
    let failing = live.ok("execute_batch", json!({"connection_id": ID, "use_transaction": true, "script": format!("INSERT INTO {scratch}.t VALUES (30, 'a'); INSERT INTO {scratch}.t VALUES (1, 'duplicate key')")}));
    assert_eq!(failing["transaction"], "rolled_back", "{failing}");
    assert_eq!(cell(&live.sql(&format!("SELECT count(*) FROM {scratch}.t WHERE a = 30"), json!({}))), "0");
}

#[test]
#[ignore = "needs TABLEX_TEST_MARIADB_URI"]
fn mariadb_pinned_set_is_literal_only_and_functions_in_a_transaction_are_changes() {
    mysql_family_set_and_functions(ConnectionKind::MariaDb, "dbengine-mariadb", "TABLEX_TEST_MARIADB_URI");
}

#[test]
#[ignore = "needs TABLEX_TEST_MYSQL_URI"]
fn mysql_pinned_set_is_literal_only_and_functions_in_a_transaction_are_changes() {
    mysql_family_set_and_functions(ConnectionKind::MySql, "dbengine-mysql", "TABLEX_TEST_MYSQL_URI");
}

/// The review's attack strings against the real server: a `SET` that carries a second assignment
/// after `NAMES`, a `--` that is no comment, a stored function in a `SET` value and in a read of a
/// transaction. None of them changes the server; the function that writes is only reached with an
/// approval, and is on the list of the commit question.
fn mysql_family_set_and_functions(kind: ConnectionKind, package: &'static str, env: &str) {
    let Ok(uri) = std::env::var(env) else {
        return;
    };
    let scratch = unique("tablex_agent_fn");
    let mut live = Live::new(kind, package, &uri, AgentAccess::ReadWrite);
    live.cleanup = vec![(format!("DROP DATABASE IF EXISTS {scratch}"), None)];
    live.ok("open_session", json!({"connection_id": ID}));
    for statement in [
        format!("CREATE DATABASE {scratch}"),
        format!("CREATE TABLE {scratch}.counter(n int)"),
        format!("INSERT INTO {scratch}.counter VALUES (0)"),
    ] {
        let outcome = live.sql(&statement, json!({}));
        assert_eq!(code(&outcome), "ok", "{statement}: {outcome:?}");
    }
    // A function that writes: created outside the policy (the user's own database objects).
    live.raw(
        Language::Sql,
        &format!("CREATE FUNCTION {scratch}.next_no() RETURNS int MODIFIES SQL DATA BEGIN UPDATE {scratch}.counter SET n = n + 1; RETURN (SELECT n FROM {scratch}.counter); END"),
        None,
    );
    let counter = |live: &Live| cell(&live.sql(&format!("SELECT n FROM {scratch}.counter"), json!({})));
    let function_exists = live
        .raw_run(&format!("SELECT {scratch}.next_no()"), None, false)
        .is_ok();
    let session = live.pin();
    let on = |statement: &str| live.in_session(&session, statement, None);
    let foreign_key_checks = cell(&on("SELECT @@session.foreign_key_checks"));
    let sql_log_bin = cell(&on("SELECT @@session.sql_log_bin"));

    // The attack strings never reach the server, whatever the access.
    for statement in [
        "SET NAMES utf8mb4, foreign_key_checks = 0",
        "SET NAMES utf8mb4, sql_log_bin = 0",
        "SET NAMES utf8mb4, GLOBAL max_connections = 1",
        "SET @a = 1--1, GLOBAL max_connections = 1",
        r#"SET @x = "\" , @y = ", foreign_key_checks = 0, @z = "x""#,
    ] {
        let outcome = on(statement);
        assert_eq!(code(&outcome), "agent.dangerous_refused", "{statement}: {outcome:?}");
    }
    for statement in ["SET NAMES gbk", "SET character_set_client = sjis"] {
        assert_eq!(code(&on(statement)), "agent.unsupported", "{statement}");
    }
    assert_eq!(cell(&on("SELECT @@session.foreign_key_checks")), foreign_key_checks);
    assert_eq!(cell(&on("SELECT @@session.sql_log_bin")), sql_log_bin);
    // A function in a SET value is refused before the server: the counter does not move.
    let before = counter(&live);
    for statement in [format!("SET @n = {scratch}.next_no()"), "SET @n = (SELECT 1)".to_owned(), "SET @n = 1 + 1".to_owned()] {
        let outcome = on(&statement);
        assert_eq!(code(&outcome), "agent.unsupported", "{statement}: {outcome:?}");
    }
    assert_eq!(counter(&live), before);
    assert_eq!(code(&on("SET NAMES utf8mb4, @n = 5")), "ok");
    assert_eq!(cell(&on("SELECT @n")), "5");

    // In a transaction a read that calls a function that is not a built-in is a change: asked, and
    // on the commit list. A built-in read is not.
    let tx = live.ok("begin_transaction", json!({"connection_id": ID}))["transaction_id"].as_str().unwrap().to_owned();
    let asked = || live.approver.asked.lock().unwrap().len();
    let start = asked();
    assert_eq!(first_cell(&tx_sql(&live, &tx, &format!("SELECT upper('a'), count(*) FROM {scratch}.counter"))), "A");
    assert_eq!(asked(), start, "a built-in is a read");
    assert!(function_exists, "the test user can create a function in its scratch database");
    {
        let probed = counter(&live).parse::<u32>().unwrap();
        let call = format!("SELECT {scratch}.next_no()");
        assert_eq!(code(&tx_sql(&live, &tx, &call)), "ok");
        assert_eq!(asked(), start + 1, "the function call was put to the user");
        let before_commit = live.approver.transactions.lock().unwrap().len();
        let committed = live.ok("commit_transaction", json!({"transaction_id": tx}));
        assert_eq!(committed["committed"], true, "{committed}");
        let transactions = live.approver.transactions.lock().unwrap();
        assert_eq!(transactions.len(), before_commit + 1);
        let listed = transactions.last().unwrap().as_ref().unwrap().1.clone().unwrap();
        assert_eq!(listed, [call], "the commit question lists the call");
        drop(transactions);
        assert_eq!(counter(&live).parse::<u32>().unwrap(), probed + 1, "it ran once, in the transaction, and committed");
    }
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

/// `lock_timeout_secs` in a pinned session leaves the session's own `lock_timeout` as it was, on the
/// path of a transaction block and on the one of a statement that cannot run in one.
#[test]
#[ignore = "needs TABLEX_TEST_PG_URI"]
fn postgresql_lock_timeout_in_a_pinned_session() {
    let Ok(uri) = std::env::var("TABLEX_TEST_PG_URI") else {
        return;
    };
    let mut live = Live::new(ConnectionKind::PostgreSql, "dbengine-postgres", &uri, AgentAccess::ReadWrite);
    let table = unique("tablex_agent_pinlt");
    live.cleanup = vec![(format!("DROP TABLE IF EXISTS {table}"), None)];
    live.ok("open_session", json!({"connection_id": ID}));
    let created = live.sql(&format!("CREATE TABLE {table}(id integer)"), json!({}));
    assert_eq!(code(&created), "ok", "{created:?}");
    let session = live.pin();
    let run = |statement: &str, extra: Value| {
        let mut args = json!({"connection_id": ID, "statement": statement, "session_id": session});
        for (key, value) in extra.as_object().into_iter().flatten() {
            args[key] = value.clone();
        }
        live.call("execute_query", args)
    };
    assert_eq!(code(&run("SET lock_timeout = '2s'", json!({}))), "ok");
    for statement in [
        format!("ALTER TABLE {table} ADD COLUMN c text"),
        format!("CREATE INDEX CONCURRENTLY {table}_i ON {table} (id)"),
    ] {
        let outcome = run(&statement, json!({"lock_timeout_secs": 5}));
        assert_eq!(code(&outcome), "ok", "{statement}: {outcome:?}");
        assert_eq!(cell(&run("SHOW lock_timeout", json!({}))), "2s", "after {statement}");
    }
    assert_eq!(cell(&live.sql("SHOW lock_timeout", json!({}))), "0", "the shared session never had it");
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

/// A second session of the live connection, outside the layer, that holds a lock for a test: it
/// is rolled back when the guard is dropped, so the test's cleanup never waits for it.
struct Holder(Arc<dyn Session>);

impl Holder {
    fn new(live: &Live, setup: &[&str], database: Option<&str>) -> Self {
        let holder = Self(live.provider.raw_session(ID));
        for statement in setup {
            holder.run(statement, database);
        }
        holder
    }

    fn run(&self, statement: &str, database: Option<&str>) {
        let limits = Limits::default();
        self.0
            .run(
                &engine_agent::ports::RunRequest {
                    language: Language::Sql,
                    text: statement,
                    database,
                    limits: &limits,
                    row_limit: 10,
                    confirmed: true,
                    read_only: false,
                },
                &CancelToken::new(),
            )
            .unwrap_or_else(|error| panic!("{statement}: {error:?}"));
    }

    fn release(&self, database: Option<&str>) {
        self.run("ROLLBACK", database);
    }
}

impl Drop for Holder {
    fn drop(&mut self) {
        let limits = Limits::default();
        let _ = self.0.run(
            &engine_agent::ports::RunRequest {
                language: Language::Sql,
                text: "ROLLBACK",
                database: None,
                limits: &limits,
                row_limit: 1,
                confirmed: true,
                read_only: false,
            },
            &CancelToken::new(),
        );
        self.0.disconnect();
    }
}

/// The message of a failed call.
fn failure_text(outcome: &ToolOutcome) -> String {
    match outcome {
        ToolOutcome::Failure(error) => error.message.clone(),
        ToolOutcome::Success(value) => panic!("expected a failure, got {value}"),
    }
}

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI"]
fn postgresql_lock_timeout_applies_fires_and_is_restored() {
    let Ok(uri) = std::env::var("TABLEX_TEST_PG_URI") else {
        return;
    };
    let mut live = Live::new(
        ConnectionKind::PostgreSql,
        "dbengine-postgres",
        &uri,
        AgentAccess::ReadWrite,
    );
    let table = unique("tablex_agent_lock");
    live.cleanup = vec![(format!("DROP TABLE IF EXISTS {table}"), None)];
    live.ok("open_session", json!({"connection_id": ID}));
    let created = live.sql(&format!("CREATE TABLE {table}(id integer PRIMARY KEY, v text)"), json!({}));
    assert_eq!(code(&created), "ok", "{created:?}");
    let setting = |live: &Live| first_cell(&live.sql("SHOW lock_timeout", json!({})));
    let before = setting(&live);
    let alter = format!("ALTER TABLE {table} ADD COLUMN c text");

    // Another session holds the table; the agent's ALTER gives up after the timeout it asked for.
    let holder = Holder::new(
        &live,
        &["BEGIN", &format!("LOCK TABLE {table} IN ACCESS EXCLUSIVE MODE")],
        None,
    );
    let started = std::time::Instant::now();
    let outcome = live.sql(&alter, json!({"lock_timeout_secs": 1}));
    let waited = started.elapsed();
    let text = failure_text(&outcome);
    assert!(text.contains("The lock timeout was hit") && text.contains("Retry later"), "{text}");
    assert!(
        waited >= std::time::Duration::from_millis(900) && waited < std::time::Duration::from_secs(8),
        "{waited:?}"
    );
    assert!(matches!(&outcome, ToolOutcome::Failure(error) if error.retryable));
    // A read waits on the lock too, and the same timeout ends it.
    let read = live.sql(&format!("SELECT count(*) FROM {table}"), json!({"lock_timeout_secs": 1}));
    assert!(failure_text(&read).contains("The lock timeout was hit"), "{read:?}");
    // A script: the statement that waits fails by name, and nothing is left set.
    let script = live.call(
        "execute_batch",
        json!({"connection_id": ID, "script": alter, "lock_timeout_secs": 1}),
    );
    let ToolOutcome::Success(script) = script else {
        panic!("{script:?}")
    };
    assert_eq!(script["ok"], false);
    assert!(script["statements"][0]["error"]["message"].as_str().unwrap().contains("The lock timeout was hit"));
    // The session has its own value back, and is the same working session.
    assert_eq!(setting(&live), before);

    // Without the parameter nothing changed: the call waits for the lock until the connection's
    // statement timeout cancels it, and the error is that one.
    live.catalog.update(ID, |connection| connection.statement_timeout_secs = Some(2));
    let outcome = live.sql(&alter, json!({}));
    let text = failure_text(&outcome);
    assert!(text.contains("statement timeout"), "{text}");
    assert!(!text.contains("lock timeout was hit"), "{text}");
    live.catalog.update(ID, |connection| connection.statement_timeout_secs = None);
    assert_eq!(setting(&live), before);

    // Once the lock is free the same call, with the parameter, goes through and the setting is
    // put back: the column is there, and a later statement has no timeout of its own.
    holder.release(None);
    let outcome = live.sql(&alter, json!({"lock_timeout_secs": 5}));
    assert_eq!(code(&outcome), "ok", "{outcome:?}");
    assert_eq!(first_cell(&live.sql(&format!("SELECT count(c) FROM {table}"), json!({}))), "0");
    assert_eq!(setting(&live), before);
    // It ran in a transaction of its own and committed: a different session sees the column.
    let seen = live
        .raw_run(&format!("SELECT count(c) FROM {table}"), None, false)
        .expect("another session reads the column");
    assert_eq!(seen.rows[0][0], "0");
    // The database of the call goes along to every step.
    let outcome = live.sql(
        &format!("ALTER TABLE {table} ADD COLUMN f text"),
        json!({"lock_timeout_secs": 5, "database": database_of(&uri)}),
    );
    assert_eq!(code(&outcome), "ok", "{outcome:?}");
    // A statement that cannot run in a transaction block takes the session path, and works.
    let index = unique("tablex_agent_idx");
    let outcome = live.sql(
        &format!("CREATE INDEX CONCURRENTLY {index} ON {table} (id)"),
        json!({"lock_timeout_secs": 5}),
    );
    assert_eq!(code(&outcome), "ok", "{outcome:?}");
    live.cleanup.push((format!("DROP INDEX IF EXISTS {index}"), None));
    assert_eq!(setting(&live), before, "RESET put the session value back");

    // In a transaction it is local to the transaction, lifted after the call, and gone at its end.
    let tx = live.ok("begin_transaction", json!({"connection_id": ID}))["transaction_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let inside = tx_sql(&live, &tx, &format!("ALTER TABLE {table} ADD COLUMN d text"));
    assert_eq!(code(&inside), "ok");
    let with = live.sql(
        &format!("ALTER TABLE {table} ADD COLUMN e text"),
        json!({"transaction_id": tx, "lock_timeout_secs": 9}),
    );
    assert_eq!(code(&with), "ok", "{with:?}");
    assert_eq!(first_cell(&tx_sql(&live, &tx, "SHOW lock_timeout")), before);
    live.ok("rollback_transaction", json!({"transaction_id": tx}));
    assert_eq!(setting(&live), before);

    // A lock timeout inside a transaction aborts it: the message says to roll back, and after the
    // rollback the session works as before.
    let holder = Holder::new(
        &live,
        &["BEGIN", &format!("LOCK TABLE {table} IN ACCESS EXCLUSIVE MODE")],
        None,
    );
    let tx = live.ok("begin_transaction", json!({"connection_id": ID}))["transaction_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let outcome = live.sql(
        &format!("ALTER TABLE {table} ADD COLUMN g text"),
        json!({"transaction_id": tx, "lock_timeout_secs": 1}),
    );
    let text = failure_text(&outcome);
    assert!(text.contains("The lock timeout was hit"), "{text}");
    assert!(text.contains("roll it back, then retry the transaction"), "{text}");
    assert_eq!(code(&tx_sql(&live, &tx, "SELECT 1")), "agent.transaction_aborted");
    live.ok("rollback_transaction", json!({"transaction_id": tx}));
    holder.release(None);
    assert_eq!(first_cell(&live.sql("SELECT 1", json!({}))), "1");
    assert_eq!(setting(&live), before);
}

#[test]
#[ignore = "needs TABLEX_TEST_MARIADB_URI"]
fn mariadb_lock_timeout_applies_fires_and_is_restored() {
    mysql_family_lock_timeout(ConnectionKind::MariaDb, "dbengine-mariadb", "TABLEX_TEST_MARIADB_URI");
}

#[test]
#[ignore = "needs TABLEX_TEST_MYSQL_URI"]
fn mysql_lock_timeout_applies_fires_and_is_restored() {
    mysql_family_lock_timeout(ConnectionKind::MySql, "dbengine-mysql", "TABLEX_TEST_MYSQL_URI");
}

fn mysql_family_lock_timeout(kind: ConnectionKind, package: &'static str, env: &str) {
    let Ok(uri) = std::env::var(env) else {
        return;
    };
    let database = database_of(&uri);
    let database = Some(database.as_str());
    let mut live = Live::new(kind, package, &uri, AgentAccess::ReadWrite);
    let table = unique("tablex_agent_lock");
    live.cleanup = vec![(format!("DROP TABLE IF EXISTS {table}"), database.map(str::to_owned))];
    live.ok("open_session", json!({"connection_id": ID}));
    let extra = json!({"database": database});
    let created = live.sql(
        &format!("CREATE TABLE {table}(id integer PRIMARY KEY, v varchar(20)) ENGINE=InnoDB"),
        extra.clone(),
    );
    assert_eq!(code(&created), "ok", "{created:?}");
    assert_eq!(code(&live.sql(&format!("INSERT INTO {table} VALUES (1, 'a')"), extra.clone())), "ok");
    let settings = |live: &Live| {
        let outcome = live.sql(
            "SELECT @@SESSION.lock_wait_timeout, @@SESSION.innodb_lock_wait_timeout",
            extra.clone(),
        );
        match outcome {
            ToolOutcome::Success(value) => value["rows"][0].to_string(),
            ToolOutcome::Failure(error) => panic!("{error:?}"),
        }
    };
    let before = settings(&live);
    let with = |secs: u64| {
        let mut args = extra.clone();
        args["lock_timeout_secs"] = json!(secs);
        args
    };
    let alter = format!("ALTER TABLE {table} ADD COLUMN c text");

    // Another session has the table open in a transaction (a metadata lock) and a row locked.
    let holder = Holder::new(
        &live,
        &[
            "START TRANSACTION",
            &format!("SELECT * FROM {table}"),
            &format!("UPDATE {table} SET v = 'held' WHERE id = 1"),
        ],
        database,
    );
    let started = std::time::Instant::now();
    let outcome = live.sql(&alter, with(1));
    let waited = started.elapsed();
    let text = failure_text(&outcome);
    assert!(text.contains("The lock timeout was hit") && text.contains("Retry later"), "{text}");
    assert!(
        waited >= std::time::Duration::from_millis(900) && waited < std::time::Duration::from_secs(8),
        "{waited:?}"
    );
    // A row lock: the InnoDB timeout is the one that fires.
    let started = std::time::Instant::now();
    let row = live.sql(&format!("UPDATE {table} SET v = 'agent' WHERE id = 1"), with(1));
    assert!(failure_text(&row).contains("The lock timeout was hit"), "{row:?}");
    assert!(started.elapsed() < std::time::Duration::from_secs(8));
    assert_eq!(settings(&live), before, "both settings are back");

    // Without the parameter: the statement timeout of the connection is what ends the wait.
    live.catalog.update(ID, |connection| connection.statement_timeout_secs = Some(2));
    let text = failure_text(&live.sql(&alter, extra.clone()));
    assert!(text.contains("statement timeout"), "{text}");
    assert!(!text.contains("lock timeout was hit"), "{text}");
    live.catalog.update(ID, |connection| connection.statement_timeout_secs = None);
    assert_eq!(settings(&live), before);

    // The holder's row was never changed, and with the lock free the call goes through.
    holder.release(database);
    assert_eq!(
        first_cell(&live.sql(&format!("SELECT v FROM {table} WHERE id = 1"), extra.clone())),
        "a"
    );
    let outcome = live.sql(&alter, with(5));
    assert_eq!(code(&outcome), "ok", "{outcome:?}");
    assert_eq!(settings(&live), before);
    // Nothing was kept in user variables (a statement of the agent could overwrite those) ...
    let leftover = match live.sql("SELECT @tablex_lock_wait_timeout, @tablex_innodb_lock_wait_timeout", extra.clone()) {
        ToolOutcome::Success(value) => value["rows"][0].to_string(),
        ToolOutcome::Failure(error) => panic!("{error:?}"),
    };
    assert_eq!(leftover, r#"["NULL","NULL"]"#);
    // ... and an agent cannot assign one to bend the restore.
    let assign = live.sql("SELECT @tablex_lock_wait_timeout := 31536000", extra.clone());
    assert_eq!(code(&assign), "agent.unsupported", "{assign:?}");
    assert_eq!(settings(&live), before);
}

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI"]
fn postgresql_lock_timeout_commit_errors_and_statements_refused_in_a_block() {
    let Ok(uri) = std::env::var("TABLEX_TEST_PG_URI") else {
        return;
    };
    let mut live = Live::new(
        ConnectionKind::PostgreSql,
        "dbengine-postgres",
        &uri,
        AgentAccess::ReadWrite,
    );
    let parent = unique("tablex_agent_fkp");
    let child = unique("tablex_agent_fkc");
    let schema = unique("tablex_agent_sch");
    let partitioned = unique("tablex_agent_part");
    live.cleanup = vec![
        (format!("DROP SCHEMA IF EXISTS {schema} CASCADE"), None),
        (format!("DROP TABLE IF EXISTS {partitioned}"), None),
        (format!("DROP TABLE IF EXISTS {child}"), None),
        (format!("DROP TABLE IF EXISTS {parent}"), None),
    ];
    live.ok("open_session", json!({"connection_id": ID}));
    for statement in [
        format!("CREATE TABLE {parent}(id integer PRIMARY KEY)"),
        format!("CREATE TABLE {child}(id integer, pid integer REFERENCES {parent}(id) DEFERRABLE INITIALLY DEFERRED)"),
        format!("CREATE SCHEMA {schema}"),
        format!("CREATE TABLE {schema}.t(a integer PRIMARY KEY)"),
        format!("CREATE TABLE {partitioned}(a integer) PARTITION BY RANGE (a)"),
        format!("CREATE TABLE {partitioned}_1 PARTITION OF {partitioned} FOR VALUES FROM (0) TO (10)"),
        format!("CREATE INDEX ON {partitioned}(a)"),
    ] {
        let outcome = live.sql(&statement, json!({}));
        assert_eq!(code(&outcome), "ok", "{statement}: {outcome:?}");
    }
    live.cleanup.insert(1, (format!("DROP TABLE IF EXISTS {partitioned}_1"), None));

    // A deferred constraint fails at the COMMIT of the wrapping transaction: that is the
    // statement's error, with nothing left of it, and the session is the same working one.
    let outcome = live.sql(&format!("INSERT INTO {child} VALUES (1, 99)"), json!({"lock_timeout_secs": 5}));
    let text = failure_text(&outcome);
    assert!(text.contains("foreign key"), "{text}");
    assert!(!text.contains("session was closed"), "{text}");
    assert!(live.layer.is_open(ID));
    assert_eq!(first_cell(&live.sql(&format!("SELECT count(*) FROM {child}"), json!({}))), "0");
    let outcome = live.sql(&format!("INSERT INTO {parent} VALUES (1)"), json!({"lock_timeout_secs": 5}));
    assert_eq!(code(&outcome), "ok", "{outcome:?}");

    // REINDEX (VERBOSE) SCHEMA is one the lexer does not list: PostgreSQL refuses it in a block,
    // and it is retried once on the session path.
    let outcome = live.sql(&format!("REINDEX (VERBOSE) SCHEMA {schema}"), json!({"lock_timeout_secs": 5}));
    assert_eq!(code(&outcome), "ok", "{outcome:?}");
    assert_eq!(first_cell(&live.sql("SHOW lock_timeout", json!({}))), "0");

    // Whether this server refuses REINDEX of a partitioned table in a block (PG 17 here).
    let refused = live
        .raw_run("BEGIN", None, false)
        .and_then(|_| live.raw_run(&format!("REINDEX TABLE {partitioned}"), None, false))
        .err();
    live.raw(Language::Sql, "ROLLBACK", None);
    eprintln!("REINDEX of a partitioned table inside a block: {refused:?}");
    let outcome = live.sql(&format!("REINDEX TABLE {partitioned}"), json!({"lock_timeout_secs": 5}));
    assert_eq!(code(&outcome), "ok", "{outcome:?}");
    assert_eq!(first_cell(&live.sql("SHOW lock_timeout", json!({}))), "0");
}
