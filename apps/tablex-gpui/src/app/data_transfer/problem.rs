//! Why a step of the wizard cannot go on or a transfer cannot start, in the words the user reads.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Problem {
    /// The connection was deleted while the wizard was open.
    ConnectionGone,
    NotSql,
    InMemorySqlite,
    NotOpen,
    CannotPage,
    NoTargetConnection,
    NoTargetNamespace,
    NoFolder,
    NoFileName,
    BatchNotNumber,
    BatchOutOfRange { max: usize },
    /// The `CREATE TABLE` of a table was found wrong.
    CreateRefused { table: String, reason: String },
    /// The words of the gate of a connection (read-only, asks for a confirmation) or of the session that could not be opened.
    Refused(String),
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConnectionGone => f.write_str("This connection no longer exists."),
            Self::NotSql => f.write_str("Data can be transferred out of the tables of SQL connections."),
            Self::InMemorySqlite => f.write_str(
                "Transferring is off for an in-memory SQLite database: the transfer would read on a second connection that sees a different, empty database.",
            ),
            Self::NotOpen => f.write_str("The connection is not open."),
            Self::CannotPage => f.write_str("This driver cannot page the result of a query."),
            Self::NoTargetConnection => f.write_str("Choose a SQL connection to write to."),
            Self::NoTargetNamespace => f.write_str("Choose the database or schema the tables go to."),
            Self::NoFolder => f.write_str("Choose the folder the files are written to."),
            Self::NoFileName => f.write_str("Name the file."),
            Self::BatchNotNumber => f.write_str("The batch size must be a number"),
            Self::BatchOutOfRange { max } => write!(f, "The batch size must be between 1 and {max}"),
            Self::CreateRefused { table, reason } => write!(f, "{table}: {reason}"),
            Self::Refused(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for Problem {}
