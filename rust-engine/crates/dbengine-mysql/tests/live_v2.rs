//! Live ABI v2 tests against a dedicated MySQL server; the test bodies are
//! shared with the sibling crate (`dbengine-mysql/tests/support/live.rs`).

// The driver is loaded with `dlopen`, and the host does not load drivers on Windows yet.
#![cfg(unix)]

#[path = "../../dbengine-mysql/tests/support/live.rs"]
mod live;

use live::Cfg;

const CFG: Cfg = Cfg {
    crate_dir: "../dbengine-mysql",
    dylib: "libdbengine_mysql.dylib",
    uri_env: "TABLEX_TEST_MYSQL_URI",
    root_uri_env: "TABLEX_TEST_MYSQL_ROOT_URI",
    driver: "mysql",
    display_name: "MySQL",
    scheme: "mysql",
    json_label: "json",
};
