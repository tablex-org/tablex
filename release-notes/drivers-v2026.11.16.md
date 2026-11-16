## Drivers

PostgreSQL 0.1.2. Update it in **Driver Manager**.

- Lists only the connection's database when the connection asks for it (TableX 0.1.7), and marks the databases the user may not connect to.

## Tools

tablex-mcp 0.1.3 (needs TableX 0.1.7). Update it in **Driver Manager → Tools**, then restart your MCP client.

- Reads the connections and saved passwords from TableX 0.1.7's `tablex.db`, behind one Keychain item, so macOS asks once and **Always Allow** holds across updates. Run **Allow Keychain access** again after updating.
- Signed with the same certificate as TableX.
