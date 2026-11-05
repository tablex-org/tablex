//! The MongoDB Shell plugin core: mongosh-style scripts on an embedded QuickJS.
//!
//! [`run_script`] runs one script against a [`CommandPort`] and returns the
//! [`ShellReply`](engine_driver_api::mongo_shell::ShellReply). The core owns no
//! MongoDB client; the `tablex-mongo-shell` executable wires the port to its
//! stdio protocol (see `engine_driver_api::mongo_shell`).
//!
//! One QuickJS runtime and context per run (fresh globals), 256 MiB memory
//! limit, bounded stack, an interrupt handler that polls the port's cancel
//! flag and the wall-clock timeout, output capped at 10 000 lines / 4 MiB.
//! `Math.random`-grade entropy only: ObjectIds and UUIDs are not for secrets.

mod ejson;
mod engine;
mod guard;
mod output;
mod port;
mod preprocess;
mod serve;
#[allow(dead_code)]
#[path = "../../engine-mongo-script/src/shell_value.rs"]
mod shell_value;

pub use engine::{run_script, run_script_with_stack, MAX_MEMORY_BYTES, MAX_STACK_BYTES};
pub use guard::{check_read_only, Refused};
pub use output::{MAX_BYTES as MAX_OUTPUT_BYTES, MAX_LINES as MAX_OUTPUT_LINES};
pub use serve::{serve, ServeError};
pub use port::{CommandError, CommandPort};
pub use preprocess::{rewrite_line_commands, Rewritten};

/// Canonical Extended JSON helpers the executable and the tests share.
pub mod text {
    pub use crate::ejson::{document_to_text, parse_document, parse_value, value_to_text, EjsonError};
}
