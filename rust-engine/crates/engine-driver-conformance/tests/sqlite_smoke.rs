//! The SQL conformance suite against the SQLite driver dylib, over a database
//! *file* in a temp directory (no server needed). Strict: every applicable
//! scenario must pass; the two that do not apply to SQLite are documented skips
//! (`SqliteFixture`: no `CREATE DATABASE`, no SQL to interrupt another session).
//!
//! ```text
//! cargo test -p engine-driver-conformance --test sqlite_smoke -- --nocapture
//! ```
//!
//! A second test proves the harness still detects violations: it runs a fixture
//! that expects the wrong driver id and identifier quote.

use engine_driver_api::sql::SqlObjectRef;
use engine_driver_conformance::fixtures::SqliteFixture;
use engine_driver_conformance::harness::Ctx;
use engine_driver_conformance::{
    build_release_dylib, run_scenarios, run_sql_conformance_report, MutationTarget, Outcome,
    Scratch, SqlFixture, SCENARIOS,
};
use serde_json::Value;
use std::path::{Path, PathBuf};

fn dylib() -> PathBuf {
    build_release_dylib(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../dbengine-sqlite/Cargo.toml"),
        "dbengine_sqlite",
        &[],
    )
    .expect("build the SQLite driver")
}

fn file_uri(dir: &tempfile::TempDir) -> String {
    format!("sqlite://{}", dir.path().join("conformance.db").display())
}

/// Scenarios SQLite does not pass yet. The SQLite driver worker removes entries
/// as it fixes them; the test fails if a listed scenario starts passing.
const PENDING: &[&str] = &[];

#[test]
fn sqlite_passes_every_applicable_scenario() {
    let dir = tempfile::TempDir::new().unwrap();
    let report = run_sql_conformance_report(&dylib(), &file_uri(&dir), &SqliteFixture);
    eprintln!("{}", report.summary());
    let failed: Vec<_> = report.failed().iter().map(|result| result.name).collect();
    assert_eq!(failed, PENDING, "{}", report.summary());
    let skipped: Vec<_> = report
        .results
        .iter()
        .filter(|result| matches!(result.outcome, Outcome::Skipped(_)))
        .map(|result| result.name)
        .collect();
    assert_eq!(
        skipped,
        [
            "query_nested_with_write",
            "query_interrupted",
            "create_database_bad_payload",
            "create_database"
        ],
        "only the scenarios that cannot apply to SQLite may be skipped"
    );
    assert_eq!(report.results.len(), SCENARIOS.len());
}

/// Delegates everything to [`SqliteFixture`] except the driver identity, which
/// it gets wrong on purpose.
struct WrongIdentity(SqliteFixture);

macro_rules! delegate {
    ($($method:ident($($arg:ident: $ty:ty),*) -> $ret:ty;)+) => {
        $(fn $method(&self, $($arg: $ty),*) -> $ret { self.0.$method($($arg),*) })+
    };
}

impl SqlFixture for WrongIdentity {
    fn label(&self) -> &str {
        "wrong"
    }
    fn driver_id(&self) -> &str {
        "postgresql"
    }
    fn identifier_quote(&self) -> &str {
        "`"
    }
    delegate! {
        supports_schemas() -> bool;
        current_database_query() -> String;
        create_scratch(s: &Scratch) -> Vec<String>;
        drop_scratch(s: &Scratch) -> Vec<String>;
        create_tables(s: &Scratch) -> Vec<String>;
        table(s: &Scratch, t: &str) -> String;
        numbers(n: u64) -> String;
        slow_query(n: u32) -> String;
        backend_id_query() -> String;
        interrupt_backend_statement(id: &str) -> String;
        write_statements(s: &Scratch) -> Vec<String>;
        write_returning_statements(s: &Scratch) -> Vec<String>;
        grant_statement(s: &Scratch) -> String;
        navigator_tables_request(s: &Scratch) -> Value;
        object_ref(s: &Scratch, t: &str) -> SqlObjectRef;
        mutation_target(s: &Scratch, t: &str) -> MutationTarget;
        drop_database(n: &str) -> Vec<String>;
    }
}

#[test]
fn the_harness_still_detects_violations() {
    let dir = tempfile::TempDir::new().unwrap();
    let fixture = WrongIdentity(SqliteFixture);
    let ctx = Ctx::load(&dylib(), &file_uri(&dir), &fixture).expect("load SQLite driver");
    let report = run_scenarios(&ctx, &["capabilities", "query_page_shape"]);
    eprintln!("{}", report.summary());
    let outcome = |name: &str| {
        report
            .results
            .iter()
            .find(|result| result.name == name)
            .unwrap_or_else(|| panic!("scenario {name} did not run"))
            .outcome
            .clone()
    };
    assert_eq!(outcome("query_page_shape"), Outcome::Passed);
    match outcome("capabilities") {
        Outcome::Failed(message) => assert!(message.contains("driver id"), "{message}"),
        other => panic!("a wrong driver id must fail the capabilities scenario: {other:?}"),
    }
    assert!(!report.is_ok());
}
