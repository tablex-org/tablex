//! What the shell needs from whoever owns the MongoDB connection.

use bson::Document;

/// Why a command produced no reply document.
#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    /// The run was stopped while the command was in flight.
    #[error("operation cancelled")]
    Cancelled,
    /// The host refused the request itself (`code` is its error code).
    #[error("{message}")]
    Refused { code: String, message: String },
    /// Transport, server or protocol trouble.
    #[error("{0}")]
    Failed(String),
}

/// Runs commands on the connection the script was opened on. The shell owns
/// no client: the stdio executable forwards to the host, the unit tests to a
/// fake. Server-side failures (`ok: 0`) are replies, not errors.
pub trait CommandPort {
    /// Runs `command` (its first key is the command name) in `database`.
    fn run_command(&self, database: &str, command: Document) -> Result<Document, CommandError>;

    /// The same on canonical Extended JSON text; the default converts around
    /// [`CommandPort::run_command`]. Ports that already hold text override it
    /// so a large reply is not rebuilt.
    fn run_command_text(&self, database: &str, command: &str) -> Result<String, CommandError> {
        let document = crate::ejson::parse_document(command)
            .map_err(|error| CommandError::Failed(error.to_string()))?;
        let reply = self.run_command(database, document)?;
        Ok(crate::ejson::document_to_text(&reply))
    }

    /// Whether the run was stopped; polled while JavaScript runs.
    fn is_cancelled(&self) -> bool {
        false
    }
}
