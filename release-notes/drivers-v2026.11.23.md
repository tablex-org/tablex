## Drivers and tools

tablex-mcp 0.1.6. Update it in **Driver Manager → Tools**. tablex 0.1.10 then asks for Keychain access once: choose **Always Allow**, then restart your MCP client. The database drivers are unchanged.

## What's new

- **Migrations through MCP.** A change that only adds (`CREATE TABLE`, `CREATE INDEX`, `ALTER TABLE … ADD COLUMN` …) is now an ordinary change on a **Read and write** connection: your MCP client asks you once and it runs. A statement that can destroy data (`DROP`, `TRUNCATE`, `ALTER … DROP COLUMN`, a `DELETE` without `WHERE` …) is asked about with its risk stated first, and runs only on a connection that is not of a production type; on a production connection it is refused and you run it in the tablex app.

## Fixes

- **tablex-mcp 0.1.6 no longer pops up Keychain dialogs while an agent is working** ([#1](https://github.com/tablex-org/tablex/issues/1)). It now reads one key in the Keychain instead of an item per connection.
  - When it is not allowed to read that key, the agent gets a clear error with the command that fixes it: `tablex-mcp --authorize-keychain`, or **Allow Keychain access** in Driver Manager.
  - Choosing **Deny** stops at once.
