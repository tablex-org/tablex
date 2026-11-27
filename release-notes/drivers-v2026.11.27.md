## Drivers and tools

Two new drivers, DuckDB 0.1.0 and SQL Server 0.1.0, and a new patch version of ClickHouse 0.1.4, MariaDB 0.1.3, MySQL 0.1.3, PostgreSQL 0.1.5 and SQLite 0.1.3. tablex-mcp is 0.1.8. Install and update them in **Driver Manager**.

## What's new

- **DuckDB** (needs tablex 0.1.12): open a `.duckdb` file or an in-memory database, and open CSV, TSV, JSON, NDJSON and Parquet files as tables. Reading Parquet and JSON is built in, with no download.
- **SQL Server** (needs tablex 0.1.12): user name and password sign-in, databases, schemas, tables and views, queries and row edits.
- **Columns of a whole schema in one request** for ClickHouse, DuckDB, MariaDB, MySQL, PostgreSQL, SQLite and SQL Server.
- **PostgreSQL 0.1.5** can count the rows an UPDATE or DELETE will change inside an open transaction.
- **tablex-mcp 0.1.8**
  - `--http` serves MCP over Streamable HTTP on a loopback port, with a token.
  - `lock_timeout_secs` on `execute_query` and `execute_batch`, so a change on a busy table gives up instead of waiting behind other locks.
  - DuckDB connections, opened with file access turned off.
  - When the MCP client cannot ask you, tablex asks in the app.
  - Refusals say which setting allows the call.

## Fixes

- **tablex-mcp:** MongoDB pipelines (`$out`, `$merge`, `$lookup`, `$graphLookup`, `$unionWith`) are held to the databases an MCP client may use.

Every driver is now licensed under the AGPL-3.0, like tablex.
