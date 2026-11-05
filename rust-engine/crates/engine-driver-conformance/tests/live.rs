//! Runs the suite against the real drivers and dedicated test servers.
//!
//! ```text
//! TABLEX_TEST_PG_URI=postgres://tablex:tablex@127.0.0.1:55432/tablex \
//! TABLEX_TEST_MYSQL_URI=mysql://tablex:tablex@127.0.0.1:53306/tablex \
//! TABLEX_TEST_MARIADB_URI=mariadb://tablex:tablex@127.0.0.1:53307/tablex \
//! TABLEX_TEST_CLICKHOUSE_URI=clickhouse://tablex:tablex@127.0.0.1:58123/tablex \
//!   cargo test -p engine-driver-conformance --test live -- --ignored --nocapture --test-threads=1
//! ```
//!
//! By default these tests only *print* the report (the failures are the driver
//! workers' worklist); set `TABLEX_CONFORMANCE_STRICT=1` to make a violation
//! fail the test. `TABLEX_CONFORMANCE_ONLY=a,b` limits the scenarios.

use engine_driver_conformance::fixtures::{
    ClickHouseFixture, MysqlFixture, PostgresFixture, SqliteFixture,
};
use engine_driver_conformance::{build_release_dylib, run_sql_conformance_report, SqlFixture};
use std::path::Path;

fn run(env_name: &str, crate_dir: &str, lib: &str, features: &[&str], fixture: &dyn SqlFixture) {
    let Ok(uri) = std::env::var(env_name) else {
        eprintln!("skipped: {env_name} is not set");
        return;
    };
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(crate_dir)
        .join("Cargo.toml");
    let dylib = build_release_dylib(&manifest, lib, features).expect("build the driver");
    let report = run_sql_conformance_report(&dylib, &uri, fixture);
    eprintln!("{}", report.summary());
    if std::env::var("TABLEX_CONFORMANCE_STRICT").is_ok() {
        assert!(report.is_ok(), "{}", report.summary());
    }
}

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL server)"]
fn postgresql() {
    run(
        "TABLEX_TEST_PG_URI",
        "dbengine-postgres",
        "dbengine_postgres",
        &["abi-v2"],
        &PostgresFixture,
    );
}

#[test]
#[ignore = "needs TABLEX_TEST_MYSQL_URI (dedicated MySQL server)"]
fn mysql() {
    run(
        "TABLEX_TEST_MYSQL_URI",
        "dbengine-mysql",
        "dbengine_mysql",
        &["abi-v2"],
        &MysqlFixture::MYSQL,
    );
}

#[test]
#[ignore = "needs TABLEX_TEST_MARIADB_URI (dedicated MariaDB server)"]
fn mariadb() {
    run(
        "TABLEX_TEST_MARIADB_URI",
        "dbengine-mariadb",
        "dbengine_mariadb",
        &["abi-v2"],
        &MysqlFixture::MARIADB,
    );
}

#[test]
#[ignore = "builds the SQLite driver; needs no server"]
fn sqlite() {
    let dir = tempfile::TempDir::new().unwrap();
    let uri = format!("sqlite://{}", dir.path().join("conformance.db").display());
    std::env::set_var("TABLEX_TEST_SQLITE_URI", uri);
    run(
        "TABLEX_TEST_SQLITE_URI",
        "dbengine-sqlite",
        "dbengine_sqlite",
        &[],
        &SqliteFixture,
    );
}

#[test]
#[ignore = "needs TABLEX_TEST_CLICKHOUSE_URI (dedicated ClickHouse server)"]
fn clickhouse() {
    run(
        "TABLEX_TEST_CLICKHOUSE_URI",
        "dbengine-clickhouse",
        "dbengine_clickhouse",
        &[],
        &ClickHouseFixture,
    );
}
