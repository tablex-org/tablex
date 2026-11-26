# Third-party licences

tablex is licensed under AGPL-3.0-only. The libraries it is built with keep their own licences; this
folder lists the ones its drivers bundle.

## Drivers

Every driver is tablex code and is released under AGPL-3.0-only, like the app. Each one is built
with the client library of its database, which keeps its own licence:

| Driver | Client library it bundles | Licence of that library |
| --- | --- | --- |
| PostgreSQL | `tokio-postgres` | MIT OR Apache-2.0 |
| MySQL, MariaDB | `mysql_async` | MIT OR Apache-2.0 |
| SQLite | `rusqlite`, SQLite | MIT; SQLite is in the public domain |
| MongoDB | `mongodb` | Apache-2.0 |
| Redis | `redis` | BSD-3-Clause |
| DuckDB | `duckdb`, DuckDB | MIT |
| SQL Server | `tiberius` (vendored in `rust-engine/vendor/tiberius`, changed) | MIT OR Apache-2.0 |
| ClickHouse | none (HTTP with `ureq`) | MIT OR Apache-2.0 |
