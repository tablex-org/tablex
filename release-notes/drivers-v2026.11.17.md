## Tools

tablex-mcp 0.1.4 and MongoDB Shell 0.1.1. Update them in **Driver Manager → Tools** (the MongoDB Shell sits under the MongoDB driver), then restart your MCP client.

## What's new

- **tablex-mcp: MongoDB queries.** `mongo_find`, `mongo_aggregate` and `mongo_count` read MongoDB collections with **Read** access or more: filters, projections, sorts and pipelines in Extended JSON, at most 1000 documents (200 by default), with `truncated` set when the result was cut. They are read-only: the MongoDB driver refuses anything that writes, `$out` and `$merge` included. A MongoDB connection no longer counts as browse-only.
- **MongoDB Shell: statement by statement.** A script runs one statement after the other, as a MongoDB shell does: a statement that does not parse fails at its own line instead of stopping the whole script before it starts, and with **On script error → Run the remaining statements** (TableX 0.1.8) the statements after a failed one still run, with its error in the Output.
