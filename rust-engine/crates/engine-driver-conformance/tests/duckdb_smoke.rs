//! The SQL conformance suite against the DuckDB driver dylib, over a database *file* in a temp
//! directory (no server needed). Strict: every applicable scenario must pass; the ones that do not
//! apply to DuckDB are documented skips (`DuckdbFixture`).
//!
//! Ignored by default: it builds the driver in release mode, which compiles DuckDB's C++ sources
//! (minutes, not seconds).
//!
//! ```text
//! cargo test -p engine-driver-conformance --test duckdb_smoke -- --ignored --nocapture
//! ```

use engine_driver_conformance::fixtures::DuckdbFixture;
use engine_driver_conformance::{
    build_release_dylib, run_sql_conformance_report, Outcome, SCENARIOS,
};
use std::path::Path;

#[test]
#[ignore = "builds the DuckDB driver in release mode (compiles DuckDB itself); needs no server"]
fn duckdb_passes_every_applicable_scenario() {
    let dylib = build_release_dylib(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../dbengine-duckdb/Cargo.toml"),
        "dbengine_duckdb",
        &[],
    )
    .expect("build the DuckDB driver");
    let dir = tempfile::TempDir::new().unwrap();
    let uri = format!("duckdb://{}", dir.path().join("conformance.duckdb").display());
    let report = run_sql_conformance_report(&dylib, &uri, &DuckdbFixture);
    eprintln!("{}", report.summary());
    let failed: Vec<_> = report.failed().iter().map(|result| result.name).collect();
    assert!(failed.is_empty(), "{}", report.summary());
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
            "mutation_zero_row_insert",
            "mutation_in_user_transaction",
            "mutation_cancelled",
            "create_database_bad_payload",
            "create_database"
        ],
        "only the scenarios that cannot apply to DuckDB may be skipped"
    );
    assert_eq!(report.results.len(), SCENARIOS.len());
}
