# tablex-mcp

The tablex MCP server: an AI agent (Claude Code, Cursor, any MCP client) uses the database connections you saved in tablex, under tablex's rules. Access is **off by default**; you turn it on for all connections in tablex > Settings > AI > Default agent access, or per connection (connection > Overrides > Agent access). A statement that changes data needs access **Read and write** and is put to you, in your client (or in the tablex app when your client cannot ask), and runs only when you accept. Each change is asked about on its own; a client set to auto-approve can answer without you seeing it, so only use clients you trust. A production connection is never changed through MCP.

It speaks MCP over **stdio** (the client starts it) or, with `--http`, over **Streamable HTTP**.

## Install

- **Driver Manager** (recommended): tablex > Driver Manager > Tools > tablex-mcp. On macOS run `tablex-mcp --authorize-keychain` once so it can read saved passwords.
- **npm** (not live yet: `@tablex/mcp-server` is prepared in `packages/mcp-server` but has not been published, so `npx` cannot find it until a maintainer publishes it): `npx -y @tablex/mcp-server` runs the same signed binary. See [npm](#npm-not-live-until-published).

`tablex-mcp --print-config` prints a ready-to-paste stdio client configuration for the copy you are running.

## stdio (default)

```json
{ "mcpServers": { "tablex": { "command": "/path/to/tablex-mcp", "args": [] } } }
```

## HTTP

```sh
tablex-mcp --http                    # http://127.0.0.1:7717/mcp
tablex-mcp --http 9000               # another port, still loopback
tablex-mcp --http 127.0.0.1:0        # any free port; the address is printed on stderr
tablex-mcp --http 0.0.0.0:7717 --allow-remote   # reachable from other machines: read the notes below
tablex-mcp --print-token             # show the bearer token
```

The server runs until it is stopped (Ctrl-C or SIGTERM); stdout stays empty and everything it says goes to stderr. It keeps stdio as the default: without `--http` nothing listens on a socket.

### The token

Every request must carry `Authorization: Bearer <token>`. The token is, in this order:

1. the environment variable `TABLEX_MCP_TOKEN` (at least 16 printable characters, no spaces), or
2. the file `mcp-http-token` in the tablex data directory (macOS `~/Library/Application Support/tablex/`, Linux `~/.local/share/tablex/`, Windows `%APPDATA%\tablex\`), created on first use with 64 random hex characters and mode `0600` (on Windows it takes the permissions of your profile folder). A file readable by other users is refused, not used (Unix).

The token is never written to a log: the start-up message says where it comes from. `tablex-mcp --print-token` prints it to stdout on request. To change it, delete the file (a new one is made on the next start) or set `TABLEX_MCP_TOKEN`.

### Client configuration

Replace the token with yours (`tablex-mcp --print-token`). Clients differ; check yours for the exact keys.

Claude Code:

```sh
claude mcp add --transport http tablex http://127.0.0.1:7717/mcp \
  --header "Authorization: Bearer $(tablex-mcp --print-token)"
```

Cursor (`~/.cursor/mcp.json`), with the token in your environment as `TABLEX_MCP_TOKEN`:

```json
{
  "mcpServers": {
    "tablex": {
      "url": "http://127.0.0.1:7717/mcp",
      "headers": { "Authorization": "Bearer ${env:TABLEX_MCP_TOKEN}" }
    }
  }
}
```

VS Code (`.vscode/mcp.json`):

```json
{
  "servers": {
    "tablex": {
      "type": "http",
      "url": "http://127.0.0.1:7717/mcp",
      "headers": { "Authorization": "Bearer <token>" }
    }
  }
}
```

A client that only speaks stdio can still use the HTTP server through a bridge that adds the header; or just use stdio.

### Security notes

- **Loopback only by default.** `--http` listens on `127.0.0.1`. An address other machines can reach (`0.0.0.0`, `::`, a LAN address) is refused unless you add `--allow-remote`, and then a warning is printed. The connection is plain HTTP: over a network the token and every query and result are readable by anyone on the path. Put TLS in front (a reverse proxy) or use an SSH tunnel (`ssh -L 7717:127.0.0.1:7717 host`) and keep the server on loopback.
- **The token is the access.** Anyone who has it can use every connection an agent may use, with the same approvals asked of whoever answers the questions. The token is compared in constant time; a missing, malformed or wrong one gets a bare `401` with no body, before the path, method or body is looked at. There is no per-client identity and no rate limit: do not expose the port to people you do not trust.
- **One tool layer, one client per session.** Two HTTP sessions use the same saved connections and the same shared database session, but each is a client of its own for transactions and pinned sessions: the `tx-` or `ses-` id one session got cannot be used, committed, rolled back or closed from another session (it answers as an id that does not exist). When a session ends (`DELETE`, or its idle timeout) its transactions are rolled back and its pinned sessions closed. At most 4 transactions and 4 pinned sessions per session, and 16 of both in all. A request that is not part of a session (a client of the newest protocol revision, served one request at a time) cannot `begin_transaction` or `open_session` with `pinned`: nothing could name what it opens later; it gets a clear refusal.
- **DNS rebinding.** After the token, `Host` must be `localhost`, `127.0.0.1` or `[::1]` (any port), or the address listened on, and a request that has an `Origin` (a browser) must come from a loopback page; anything else is `403`. On `0.0.0.0` or `::` this machine's own addresses and names are not accepted either (the server prints a note at start): with `--allow-remote` add every name or address clients use in `TABLEX_MCP_ALLOWED_HOSTS` (comma separated, `name` or `name:port`) and any page origin in `TABLEX_MCP_ALLOWED_ORIGINS` (`https://host:port`, `:*` for any port).
- **Limits.** A request body is at most 8 MiB (`413`), and must arrive within 30 s (`408`); at most 16 sessions (`503` to a new one) and 64 connections; a connection that does not send its headers in 10 s is dropped; a session nobody used for 30 minutes is closed (its client gets `404` and starts a new one). The tool limits (statement size, rows, 16 concurrent calls in all) are the same as over stdio.
- **Approvals.** A write is put to the user with MCP elicitation, to the client of the session that made the call and no other (two sessions asked at once get their own questions). A client that does not declare the elicitation capability, including a client of the newest protocol revision that works without a session, gets the question in the tablex app instead (a dialog, when the app is running), exactly as over stdio; if the app is not running either, the write is refused with a message that says what to do, and nothing is run unasked.
- **Audit.** Every audit line (`audit.jsonl`) records `transport` (`stdio` or `http`) and `client`, the name the client gave itself at the handshake (a hint the client says, not a verified identity).

### Building without HTTP

The HTTP transport is the Cargo feature `http`, on by default: it adds about 1.4 MB (12%) to the stripped Linux x86_64 binary. `cargo build --profile dist -p tablex-mcp --no-default-features` builds the stdio-only server; it answers `--http` and `--print-token` with an error.

## What an agent may do on a connection

| Where | Key | Meaning |
|---|---|---|
| connection, Overrides | `agent.access` | `off`, `schema`, `read` or `read_write` (the Agent access control) |
| `settings.json` | `ai.agent_access` | the default for connections that are not of a production type and set nothing themselves; up to `read_write`, which is Read and write (each change is still put to you) |
| `settings.json` | `ai.mcp_writes` | deprecated: tablex no longer has this switch and keeps writing `true` for older `tablex-mcp` versions. This version honours it only in a file written by an older tablex (`schema_version` below 7), where `false` still blocks changes |

## Limits that need no screen of their own

These are read from the files tablex already keeps. The app does not have controls for them yet:
edit `settings.json` (the app keeps these two keys as they are when it saves its own settings).

### Only some tools: `ai.mcp_tools` in `settings.json`

```json
{ "schema_version": 4, "ai": { "mcp_tools": ["list_connections", "open_session", "list_tables", "describe_table", "execute_query"] } }
```

* Absent, or `null`: every tool is offered.
* A list of tool names: only those are offered in `tools/list` and run. Any other call is refused
  with `agent.access_denied`, whatever the connection's access. Resources follow the tool they
  stand for (`tablex://connections` is `list_connections`, a schema is `get_schema_context`, a
  table list is `list_tables`).
* An empty list allows no tool. A value that is not a list of strings allows no tool either: a typo
  narrows the agent, it never widens it.

### Only some databases of a connection

Either on the connection (its `overrides_json`, the same place as `agent.access`):

```json
{ "agent.access": "read", "agent.databases": "shop,reports" }
```

or in `settings.json`, by connection id or name (any case), for a person who edits one file:

```json
{ "ai": { "mcp_databases": { "Orders DB": ["shop", "reports"], "7b1d3f0e-3a55-4d8e-9a54-0c6f2f4e9a11": ["analytics"] } } }
```

The connection's own `agent.databases` wins when both are set. Absent: every database. A value
that is not a list of names (or not text, in the override) allows no database.

With a limit set, every tool that works inside a database must name one in `database`, and it must
be on the list; `list_namespaces` shows only those; `USE` in a pinned session may only name one.
This is a limit on what the *agent is allowed to name*. MySQL, MariaDB and ClickHouse can also
reach another database by qualifying a table (`other.table`) inside a statement: the database
user's own grants are what stop that, so give the connection a user that can only see them.

### Notes and folders in `list_connections`

`connection.notes` in the connection's overrides is a note the agent is shown with the connection
(at most 1000 characters). The folders come from the sidebar. The endpoint (`host:port/database`,
never a user, password or file path) is read from the saved address.

## Pinned sessions

`open_session` with `pinned: true` gives the agent a connection of its own that keeps `USE` and a
few `SET`s between calls (see the tool description). At most 4 are open, one nobody calls for ten
minutes is closed, and only the MCP client that opened it can use or close it.

## npm (not live until published)

> **Not published yet.** Nothing below works until a maintainer has published `@tablex/mcp-server` (see `packages/mcp-server` and the `npm-packages` job of `.github/workflows/release-drivers.yml`).

`@tablex/mcp-server` is a small Node launcher with no dependencies. npm installs the native binary from one optional package for your machine (`@tablex/mcp-darwin-arm64`, `-darwin-x64`, `-linux-x64-gnu`, `-linux-arm64-gnu`, `-win32-x64`, `-win32-arm64`): the same bytes as the signed release, checked against a SHA-256 baked into the launcher package.

```json
{ "mcpServers": { "tablex": { "command": "npx", "args": ["-y", "@tablex/mcp-server"] } } }
```

HTTP through npx: `npx -y @tablex/mcp-server --http` (arguments go to the binary unchanged).

`TABLEX_MCP_BIN=/path/to/tablex-mcp` runs a binary of your own instead; installing with `--no-optional` leaves no binary and the launcher says so.
