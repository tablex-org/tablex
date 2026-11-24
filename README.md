<img src=".github/logo.png" align="right" width="80" height="80" alt="tablex" />

# tablex

A performance-first database client written in Rust with [GPUI](https://www.gpui.rs/).

- One native app aimed at macOS, Linux and Windows. It is developed and tested on macOS; the Linux and Windows builds are wired into CI but are not proven yet.
- Database support comes from driver plugins that the app downloads, verifies and loads at run time.
- Results travel as Apache Arrow from the driver to a virtualized grid.

![tablex: a PostgreSQL query and its results](.github/screenshots/sql-results.png)

## Databases

PostgreSQL, MySQL, MariaDB, SQLite, MongoDB, Redis and ClickHouse. Each is a separate driver, installed from **Driver Manager** inside the app.

## Features

- Connection tree with folders, search, SSH tunnels and import from other clients.
- Data grid with in-place editing, staged changes, filters, cell and row detail panes, copy and export.
- SQL and MongoDB editors with completion, query history and saved scripts.
- Tabs, split panes with drag and drop, a command palette, table and database diagrams.
- An AI assistant (Anthropic, OpenAI, OpenAI-compatible servers, Ollama) that reads the schema and runs queries with the access you give each connection; every statement that changes data asks first.
- `tablex-mcp`, a standalone MCP server over your saved connections, installed from Driver Manager → Tools. On macOS, run `tablex-mcp --authorize-keychain` once (or use Allow Keychain access there) so it can read saved passwords without prompting; run it once after installing, and again only if macOS asks after an update (releases are signed with a stable certificate, so an Always Allow normally survives updates).

## Screenshots

<table>
  <tr>
    <td><img src=".github/screenshots/mongodb.png" alt="A MongoDB collection in the grid" /></td>
    <td><img src=".github/screenshots/diagram.png" alt="A database diagram" /></td>
  </tr>
  <tr>
    <td align="center">MongoDB documents in the same grid</td>
    <td align="center">Database diagram</td>
  </tr>
  <tr>
    <td><img src=".github/screenshots/assistant.png" alt="The AI assistant answering from the schema" /></td>
    <td><img src=".github/screenshots/split-panes.png" alt="Two editors side by side" /></td>
  </tr>
  <tr>
    <td align="center">AI assistant that reads the schema</td>
    <td align="center">Split panes</td>
  </tr>
</table>

The screenshots use the app's built-in sample data.

## Download

macOS 12 or later: the `.dmg` for your Mac (Apple silicon or Intel) from the [latest release](https://github.com/tablex-org/tablex/releases/latest). Drag tablex to Applications. The app is not notarized yet, so confirm the first launch with right-click > **Open**. Databases are added from **Driver Manager** inside the app.

**Linux (preview)**: `tablex-<version>-linux-x86_64` or `-linux-aarch64`, as a `.tar.gz` or an `.AppImage`, from the [latest release](https://github.com/tablex-org/tablex/releases/latest). It needs glibc 2.31 or newer (Debian 11, Ubuntu 20.04, RHEL 9 and later), a Wayland or X11 session, and a Vulkan or OpenGL driver. For the AppImage run `chmod +x` on it and start it; the tarball runs from where you unpack it.

**Windows (preview)**: for Windows 10 or 11 on x86_64, from the same release. Run `tablex-<version>-windows-x86_64-setup.exe`: it installs tablex for your user (no administrator rights, nothing else to install), adds it to the Start menu and Apps > Installed apps, and a newer installer upgrades it in place; uninstalling keeps your connections and settings. Or take the portable `tablex-<version>-windows-x86_64.zip`, unzip it and run `tablex.exe`. Neither is code signed yet, so SmartScreen asks for a confirmation the first time: **More info** > **Run anyway**.

The Linux and Windows builds do not update themselves: the app tells you about a new release and opens its page.

## Building from source

Requirements: a recent stable Rust toolchain; on Linux the usual GPUI system packages (Wayland / X11, fontconfig, Vulkan).

```bash
# the app
cargo run -p tablex-gpui

# the drivers, built and packaged for local use (the debug app reads dist/drivers/)
rust-engine/scripts/dev-drivers.sh
```

Release-profile build and the macOS bundle:

```bash
cargo build --profile dist -p tablex-gpui
scripts/bundle-macos.sh
```

## Tests

```bash
cargo test -p tablex-gpui -p tablex-mcp
cargo test --manifest-path rust-engine/Cargo.toml
python3 -m unittest discover scripts/tests
```

Tests that need a live database are ignored by default; they read `TABLEX_TEST_*` connection strings. `rust-engine/scripts/README.md` lists the scripts that start the throwaway SSH and TLS servers some of them use.

## Layout

| Path | What |
| --- | --- |
| `apps/tablex-gpui` | The desktop app |
| `apps/tablex-mcp` | The MCP server |
| `rust-engine/crates/dbengine-*` | Driver plugins |
| `rust-engine/crates/engine-*` | Driver host, driver kit, connections, AI and agent layers |
| `rust-engine/scripts`, `scripts` | Driver packaging, release and development tooling |

## Licence

AGPL-3.0, see `LICENSE`. Code adapted from other projects keeps its own licence; see `licenses/`.

## Feedback

Use GitHub issues for bug reports, feature requests and development discussion.
