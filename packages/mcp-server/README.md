# @tablex/mcp-server

`tablex-mcp` for npm: the [tablex](https://github.com/tablex-org/tablex) MCP server, which lets an AI agent use the database connections you saved in tablex under tablex's access rules (off until you enable it; a change to your data asks you first).

This package is a small launcher (no dependencies). The server itself is a native binary that npm installs from one optional package for your machine (`@tablex/mcp-darwin-arm64`, `-darwin-x64`, `-linux-x64-gnu`, `-linux-arm64-gnu`, `-win32-x64`, `-win32-arm64`): the same signed binary that tablex's Driver Manager installs.

You need tablex installed and a connection saved in it. Access is off until you turn it on in tablex > Settings > AI.

## Use

```json
{
  "mcpServers": {
    "tablex": { "command": "npx", "args": ["-y", "@tablex/mcp-server"] }
  }
}
```

Over HTTP instead of stdio (see the full notes in the [tablex-mcp README](https://github.com/tablex-org/tablex/tree/main/apps/tablex-mcp)):

```sh
npx -y @tablex/mcp-server --http          # http://127.0.0.1:7717/mcp, bearer token required
npx -y @tablex/mcp-server --print-token   # show the token
```

Every argument goes to the binary unchanged; `--help` lists them.

## Environment

- `TABLEX_MCP_BIN`: run this binary instead of the installed one (a build of your own).
- `TABLEX_MCP_NO_VERIFY=1`: do not check the installed binary against the checksum this package was published with.

## If it does not start

- "The package @tablex/mcp-... is not installed": the install skipped optional packages (`--no-optional`, `--omit=optional`). Install again without that flag.
- "no build for ...": there is no binary for your system; build `tablex-mcp` from source and set `TABLEX_MCP_BIN`.

AGPL-3.0, see `LICENSE`.
