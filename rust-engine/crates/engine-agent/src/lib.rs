//! The agent tool layer: what an AI agent may do with a TableX connection,
//! behind one policy, shared by the MCP server and the in-app assistant.
//!
//! * **Tools** (ten, names stable): `list_connections`, `open_session`,
//!   `close_session`, `list_namespaces`, `list_tables`, `describe_table`,
//!   `get_schema_context`, `execute_query`, `explain_query`,
//!   `execute_redis_command`. Typed requests and results live in [`tools`],
//!   their JSON Schemas in [`schema`].
//! * **One entry**: [`ToolLayer::call`] takes a tool name and JSON arguments
//!   and returns a [`ToolOutcome`] (a result, or a typed error whose
//!   `code` is stable: `agent.access_denied`, `agent.write_refused`,
//!   `agent.dangerous_refused`, `agent.multi_statement`, `agent.limit`,
//!   `agent.unknown_tool`, `agent.invalid_arguments`, `agent.not_connected`,
//!   `agent.cancelled`, `agent.unsupported`, or the driver's own code).
//! * **Policy**: [`AgentAccess`] (`off` < `schema_only` < `read` <
//!   `read_write`) combined with the connection type by
//!   [`AgentAccess::effective`]; every statement is classified with the
//!   drivers' own classifier ([`classify`]) and judged by [`policy`].
//! * **Front ends implement** [`ConnectionCatalog`], [`SessionProvider`]
//!   (credentials, tunnel, driver loading) and optionally [`WriteApprover`];
//!   [`DriverSessionAdapter`] is the one real [`Session`].
//! * **Limits** ([`Limits`]), the **schema digest** ([`context`]) and the
//!   **audit log** ([`audit`]) are part of the layer.
//!
//! No keychain, no tunnel code, no UI, no network client.

pub mod access;
pub mod audit;
pub mod cancel;
pub mod classify;
pub mod context;
pub mod driver_session;
pub mod error;
pub mod functions;
mod layer;
pub mod limits;
pub mod policy;
pub mod ports;
pub mod redact;
pub mod result;
pub mod sanitize;
pub mod schema;
pub mod secret;
pub mod table;
pub mod testing;
pub mod tools;

pub use access::{AgentAccess, ConnectionTypeInfo};
pub use audit::{read_entries, AuditConfig, AuditEntry, AuditLog, AuditRead, SyncPolicy};
pub use cancel::{CancelOnDrop, CancelToken};
pub use context::{CharsPerThree, SchemaContext, TokenEstimator};
pub use driver_session::DriverSessionAdapter;
pub use error::{AgentError, ToolError};
pub use layer::{ToolLayer, ToolOutcome};
pub use limits::Limits;
pub use ports::{
    ApprovalDecision, ConnectionCatalog, ConnectionInfo, ConnectionKind, Language, OpenGeneration,
    RefuseWrites, Revision, Session, SessionProvider, WriteApproval, WriteApprover,
};
pub use schema::ToolSpec;
pub use secret::SecretText;
pub use tools::ToolName;
