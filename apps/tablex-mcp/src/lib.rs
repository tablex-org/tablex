//! `tablex-mcp`: the `engine-agent` tool layer as a Model Context Protocol server over stdio.
//!
//! Nothing here decides what an agent may do: [`setup`] wires the layer to the saved connections
//! (`engine-connect`), [`server`] maps `tools/list` and `tools/call` onto it, [`transport`] runs it
//! over a byte pipe and tears the sessions down when the client goes away. **stdout is the
//! protocol**: in serve mode nothing but the MCP transport writes to it.

pub mod approver;
pub mod cli;
pub mod server;
pub mod setup;
pub mod transport;

/// The name the server reports in `initialize`, in the audit log's `front_end` and in the
/// generated client configuration.
pub const NAME: &str = "tablex-mcp";
