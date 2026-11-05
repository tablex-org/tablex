//! Live ABI v2 tests against a dedicated MariaDB server; the test bodies are
//! shared with the sibling crate (`dbengine-mysql/tests/support/live.rs`).

// The driver is loaded with `dlopen`, and the host does not load drivers on Windows yet.
#![cfg(unix)]

#[path = "../../dbengine-mysql/tests/support/live.rs"]
mod live;

use live::Cfg;

const CFG: Cfg = Cfg {
    crate_dir: "../dbengine-mariadb",
    dylib: "libdbengine_mariadb.dylib",
    uri_env: "TABLEX_TEST_MARIADB_URI",
    root_uri_env: "TABLEX_TEST_MARIADB_ROOT_URI",
    driver: "mariadb",
    display_name: "MariaDB",
    scheme: "mariadb",
    json_label: "text",
};
