//! Typed, model-readable errors with stable codes.

use serde::Serialize;
use thiserror::Error;

pub const ACCESS_DENIED: &str = "agent.access_denied";
pub const WRITE_REFUSED: &str = "agent.write_refused";
pub const DANGEROUS_REFUSED: &str = "agent.dangerous_refused";
pub const MULTI_STATEMENT: &str = "agent.multi_statement";
pub const LIMIT: &str = "agent.limit";
pub const UNKNOWN_TOOL: &str = "agent.unknown_tool";
pub const INVALID_ARGUMENTS: &str = "agent.invalid_arguments";
pub const NOT_CONNECTED: &str = "agent.not_connected";
pub const CANCELLED: &str = "agent.cancelled";
pub const BLOCKED_FUNCTION: &str = "agent.blocked_function";
pub const AUDIT_UNAVAILABLE: &str = "agent.audit_unavailable";
pub const UNSUPPORTED: &str = "agent.unsupported";
pub const TRANSACTION_GONE: &str = "agent.transaction_gone";
pub const TRANSACTION_ABORTED: &str = "agent.transaction_aborted";
pub const INTERNAL: &str = "agent.internal";

/// Why a tool call did not produce a result. Every variant has a stable
/// [`code`](Self::code); the `Display` text is written for the model: it says
/// what was refused and what would change the answer.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AgentError {
    #[error("{0}")]
    AccessDenied(String),
    #[error("{0}")]
    WriteRefused(String),
    #[error("{0}")]
    DangerousRefused(String),
    #[error(
        "only one statement per call is allowed; split the script and call the tool once per statement"
    )]
    MultiStatement,
    /// A function or clause that reaches outside the tables (files, network,
    /// server state). Refused at every access level.
    #[error(
        "`{0}` is blocked for agents: it can read server files, reach the network or change server state"
    )]
    BlockedFunction(String),
    /// The audit record of a write could not be written, so the write was not run.
    #[error(
        "the audit log cannot be written, so writes are refused until it can; reads still work"
    )]
    AuditUnavailable,
    #[error("{0}")]
    Limit(String),
    #[error("unknown tool `{0}`")]
    UnknownTool(String),
    #[error("invalid arguments: {0}")]
    InvalidArguments(String),
    #[error("{0}")]
    NotConnected(String),
    #[error("the statement was cancelled")]
    Cancelled,
    #[error("{0}")]
    Unsupported(String),
    /// The transaction is not open any more (unknown id, rolled back after the idle timeout, or
    /// with its session) or is not usable.
    #[error("{0}")]
    TransactionGone(String),
    /// PostgreSQL: an earlier statement failed, so the transaction only accepts a rollback.
    #[error("{0}")]
    TransactionAborted(String),
    /// A driver error, passed through (its message is redacted before it
    /// leaves the tool layer).
    #[error("{message}")]
    Driver {
        code: String,
        message: String,
        retryable: bool,
    },
    #[error("{0}")]
    Internal(String),
}

impl AgentError {
    pub fn code(&self) -> &str {
        match self {
            Self::AccessDenied(_) => ACCESS_DENIED,
            Self::WriteRefused(_) => WRITE_REFUSED,
            Self::DangerousRefused(_) => DANGEROUS_REFUSED,
            Self::MultiStatement => MULTI_STATEMENT,
            Self::BlockedFunction(_) => BLOCKED_FUNCTION,
            Self::AuditUnavailable => AUDIT_UNAVAILABLE,
            Self::Limit(_) => LIMIT,
            Self::UnknownTool(_) => UNKNOWN_TOOL,
            Self::InvalidArguments(_) => INVALID_ARGUMENTS,
            Self::NotConnected(_) => NOT_CONNECTED,
            Self::Cancelled => CANCELLED,
            Self::Unsupported(_) => UNSUPPORTED,
            Self::TransactionGone(_) => TRANSACTION_GONE,
            Self::TransactionAborted(_) => TRANSACTION_ABORTED,
            Self::Driver { code, .. } => code,
            Self::Internal(_) => INTERNAL,
        }
    }

    pub fn retryable(&self) -> bool {
        matches!(
            self,
            Self::Driver {
                retryable: true,
                ..
            }
        )
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::InvalidArguments(message.into())
    }

    pub fn driver(code: impl Into<String>, message: impl Into<String>, retryable: bool) -> Self {
        Self::Driver {
            code: code.into(),
            message: message.into(),
            retryable,
        }
    }
}

/// The wire form of a failed tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}
