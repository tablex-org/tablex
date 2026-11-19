## Drivers and tools

SQLite 0.1.1, MongoDB 0.2.4, PostgreSQL 0.1.3 and tablex-mcp 0.1.5. Update them in **Driver Manager** (tablex-mcp under **Tools**), then restart your MCP client. The other drivers and tools are unchanged.

## Fixes

- **SQLite 0.1.1** bundles a newer SQLite with a published security fix.
- **MongoDB 0.2.4** uses a newer DNS resolver for `mongodb+srv://` connections, with published security fixes, and a newer spreadsheet writer for Excel exports.
- **PostgreSQL 0.1.3** uses a newer spreadsheet writer for Excel exports, with published security fixes.
- **tablex-mcp 0.1.5** moves to a newer version of its MCP library with published security fixes. Nothing changes in how it works.
