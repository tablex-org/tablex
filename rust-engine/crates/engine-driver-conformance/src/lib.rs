//! Black-box conformance suite for SQL drivers.
//!
//! [`run_sql_conformance`] loads a driver dylib through the real host
//! (`engine_driver_host::DriverLoader`), connects to a live server and checks
//! every rule of `engine_driver_api::sql` ("Conformance rules"): capabilities,
//! navigator payloads and paging, query paging/not-pageable/confirmation,
//! cancel versus interrupt, mutations (null keys, batches, savepoints inside a
//! user transaction), `create_database` and `describe_object`.
//!
//! A driver supplies only its dialect-specific SQL through [`SqlFixture`];
//! [`fixtures`] has ready-made ones for PostgreSQL and MySQL/MariaDB. Typical
//! use, from a driver crate's `#[ignore]`d live test (this crate is a
//! dev-dependency only):
//!
//! ```ignore
//! use engine_driver_conformance::{build_release_dylib, fixtures::PostgresFixture, run_sql_conformance};
//!
//! #[test]
//! #[ignore = "needs TABLEX_TEST_PG_URI"]
//! fn sql_conformance() {
//!     let Ok(uri) = std::env::var("TABLEX_TEST_PG_URI") else { return };
//!     let dylib = build_release_dylib(
//!         &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"),
//!         "dbengine_postgres",
//!         &["abi-v2"],
//!     )
//!     .unwrap();
//!     run_sql_conformance(&dylib, &uri, &PostgresFixture);
//! }
//! ```
//!
//! Every scenario creates its own scratch schema/database, drops it afterwards
//! and reports pass/fail/skip independently: [`run_sql_conformance_report`]
//! returns the whole [`ConformanceReport`] (use it to see *all* violations at
//! once), [`run_sql_conformance`] panics with it. Set
//! `TABLEX_CONFORMANCE_ONLY=query_paging,mutation_batch` to run a subset.
//!
//! The server must be a **dedicated test server** (the suite creates and drops
//! databases/schemas and runs deliberately slow queries).

pub mod build;
pub mod fixture;
pub mod fixtures;
pub mod harness;
pub mod report;
pub mod scenarios;

pub use build::build_release_dylib;
pub use fixture::{MutationTarget, Scratch, SqlFixture};
pub use report::{
    run_scenarios, run_sql_conformance, run_sql_conformance_report, ConformanceReport, Outcome,
    ScenarioResult,
};
pub use scenarios::SCENARIOS;
