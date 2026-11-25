## Drivers and tools

Every database driver gets a new patch version (ClickHouse 0.1.3, MariaDB 0.1.2, MongoDB 0.2.5, MySQL 0.1.2, PostgreSQL 0.1.4, Redis 0.1.1, SQLite 0.1.2), and tablex-mcp is 0.1.7. Update them in **Driver Manager**.

## What's new

- **Windows drivers.** The drivers, tablex-mcp and the plugins are now built for Windows x86_64 too, alongside macOS and Linux (x86_64 and aarch64). tablex 0.1.11 for Linux and Windows installs them from Driver Manager.
- **tablex-mcp 0.1.7 on Linux without a keyring** follows your choice in tablex (**Settings → Security**). With the private key file it reads your saved passwords from it. With **Don't save passwords**, adding or copying a connection saves no password, says so, and tablex asks for it when connecting.

## Fixes

- Rebuilt with dependency security updates (tokio among them) and Apache Arrow 60.
