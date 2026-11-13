## Tools

tablex-mcp 0.1.2. Update it in **Driver Manager → Tools**, then restart your MCP client.

## What's new

- `tablex-mcp --authorize-keychain` (or **Allow Keychain access** in Driver Manager → Tools, TableX 0.1.6) reads the saved passwords of the connections agents may use, so macOS asks once and you can choose **Always Allow**. Run it again after an update. When the Keychain refuses during a session, the error says what to run.
- Transactions on PostgreSQL and SQLite connections with **Read and write** access: `begin_transaction`, `commit_transaction` and `rollback_transaction`, with `execute_query` taking a `transaction_id`. Each change is approved in your MCP client, and so is the commit, which lists the changes. A transaction left unused for 5 minutes, or open when the client disconnects, is rolled back.
- `list_routines` and `get_routine_source` read the definitions of functions, procedures, triggers and events with Schema access.
- The default agent access from **Settings → AI** in TableX 0.1.6 applies to the MCP server too.
