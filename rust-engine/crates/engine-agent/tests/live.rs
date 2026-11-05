//! Live tests against the dedicated test containers, through the real
//! drivers. Ignored by default and gated on the environment:
//!
//! ```text
//! TABLEX_TEST_PG_URI=postgres://tablex:tablex@127.0.0.1:55432/tablex \
//! TABLEX_TEST_MYSQL_URI=mysql://tablex:tablex@127.0.0.1:53306/tablex \
//! TABLEX_TEST_REDIS_URI=redis://127.0.0.1:56379 \
//! cargo test --manifest-path rust-engine/Cargo.toml -p engine-agent --test live -- --ignored
//! ```
//!
//! Only uniquely named scratch tables / prefixed keys are created, and they
//! are dropped / deleted even when a test fails.

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
            ToolLayer::new(catalog.clone(), provider, "live-test").with_approver(approver.clone()),
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
