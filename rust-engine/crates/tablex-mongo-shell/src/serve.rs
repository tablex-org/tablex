//! The executable's side of the protocol (`engine_driver_api::mongo_shell`):
//! handshake, one `run`, then command / reply pairs until `done`.

use crate::port::{CommandError, CommandPort};
use engine_driver_api::mongo_shell::{
    read_frame, write_frame, CommandCall, CommandFailureKind, FromChild, FrameError, ShellFailure,
    ShellReply, ShellResult, ToChild, FailureKind, MAX_FRAME_BYTES, PROTOCOL_VERSION,
};
use serde_json::value::RawValue;
use std::cell::RefCell;
use std::io::{Read, Write};

/// Why the session ended without a `done` frame.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error("the host closed the pipe before sending a run request")]
    NoRequest,
    #[error("the host sent a {0} frame where a run request was expected")]
    UnexpectedFrame(&'static str),
    #[error("the script thread panicked")]
    Panicked,
}

struct StdioPort<'a, R, W> {
    pipes: RefCell<(&'a mut R, &'a mut W)>,
    next_id: RefCell<u64>,
}

impl<R: Read, W: Write> CommandPort for StdioPort<'_, R, W> {
    fn run_command(
        &self,
        database: &str,
        command: bson::Document,
    ) -> Result<bson::Document, CommandError> {
        let text = self.run_command_text(database, &crate::ejson::document_to_text(&command))?;
        crate::ejson::parse_document(&text).map_err(|error| CommandError::Failed(error.to_string()))
    }

    fn run_command_text(&self, database: &str, command: &str) -> Result<String, CommandError> {
        let command = RawValue::from_string(command.to_owned())
            .map_err(|error| CommandError::Failed(format!("the command is not JSON: {error}")))?;
        let id = {
            let mut next = self.next_id.borrow_mut();
            *next += 1;
            *next
        };
        let mut pipes = self.pipes.borrow_mut();
        let call = FromChild::Command(CommandCall {
            id,
            database: database.to_owned(),
            command,
        });
        write_frame(&mut *pipes.1, &call)
            .map_err(|error| CommandError::Failed(format!("the host is gone: {error}")))?;
        let frame: Option<ToChild> = read_frame(&mut *pipes.0)
            .map_err(|error| CommandError::Failed(format!("bad reply from the host: {error}")))?;
        let Some(ToChild::Reply(reply)) = frame else {
            return Err(CommandError::Failed(
                "the host stopped answering commands".into(),
            ));
        };
        if reply.id != id {
            return Err(CommandError::Failed(format!(
                "the host answered command {} while {id} was pending",
                reply.id
            )));
        }
        match (reply.ok, reply.error) {
            (Some(document), None) => Ok(document.get().to_owned()),
            (None, Some(failure)) => Err(match failure.kind {
                CommandFailureKind::Cancelled => CommandError::Cancelled,
                CommandFailureKind::Refused => CommandError::Refused {
                    code: failure.code.unwrap_or_default(),
                    message: failure.message,
                },
                CommandFailureKind::Failed => CommandError::Failed(failure.message),
            }),
            _ => Err(CommandError::Failed(
                "the host's reply has neither a document nor an error".into(),
            )),
        }
    }
}

/// Stack of the thread scripts run on, and the share QuickJS may use of it.
const SCRIPT_THREAD_STACK_BYTES: usize = 16 * 1024 * 1024;
const SCRIPT_STACK_LIMIT_BYTES: usize = 8 * 1024 * 1024;

/// Serves one run over `reader` / `writer` (the child's stdin / stdout).
pub fn serve<R: Read + Send, W: Write + Send>(reader: &mut R, writer: &mut W) -> Result<(), ServeError> {
    write_frame(
        writer,
        &FromChild::Hello {
            protocol: PROTOCOL_VERSION,
        },
    )?;
    let request = match read_frame::<ToChild>(reader)? {
        Some(ToChild::Run(request)) => request,
        Some(ToChild::Reply(_)) => return Err(ServeError::UnexpectedFrame("reply")),
        None => return Err(ServeError::NoRequest),
    };
    // QuickJS recurses on the native stack: run on a thread with a known, large
    // one (the process's main thread is only 1 MiB on Windows).
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .name("tablex-shell-script".into())
            .stack_size(SCRIPT_THREAD_STACK_BYTES)
            .spawn_scoped(scope, || run_and_report(&request, reader, writer))
            .map_err(|error| ServeError::Frame(FrameError::Io(error)))?
            .join()
            .unwrap_or(Err(ServeError::Panicked))
    })
}

/// A reply that would not fit one protocol frame becomes a typed limit error
/// that keeps what the script printed, instead of a frame the host must refuse.
fn fit_reply(reply: ShellReply) -> ShellReply {
    let size = serde_json::to_vec(&reply).map_or(u64::MAX, |bytes| bytes.len() as u64);
    if size <= MAX_FRAME_BYTES {
        return reply;
    }
    ShellReply {
        output: reply.output,
        output_truncated: reply.output_truncated,
        result: ShellResult::None,
        database_after: reply.database_after,
        error: Some(ShellFailure {
            kind: FailureKind::Limit,
            message: format!(
                "the result is larger than the {} MiB the shell can hand over; narrow it with limit(), a projection or a $match / $limit stage",
                MAX_FRAME_BYTES >> 20
            ),
            line: None,
            column: None,
            code: None,
            code_name: None,
        }),
    }
}

fn run_and_report<R: Read, W: Write>(
    request: &engine_driver_api::mongo_shell::ShellRequest,
    reader: &mut R,
    writer: &mut W,
) -> Result<(), ServeError> {
    let port = StdioPort {
        pipes: RefCell::new((reader, writer)),
        next_id: RefCell::new(0),
    };
    let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        crate::engine::run_script_with_stack(request, &port, SCRIPT_STACK_LIMIT_BYTES)
    }));
    let reply = ran.unwrap_or_else(|_| ShellReply {
        output: Vec::new(),
        output_truncated: false,
        result: ShellResult::None,
        database_after: request.database.clone(),
        error: Some(ShellFailure {
            kind: FailureKind::Internal,
            message: "the shell stopped on an internal error".into(),
            line: None,
            column: None,
            code: None,
            code_name: None,
        }),
    });
    let writer = &mut *port.pipes.borrow_mut().1;
    write_frame(writer, &FromChild::Done(fit_reply(reply)))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_driver_api::mongo_shell::RawDocument;

    fn reply(documents: Vec<RawDocument>) -> ShellReply {
        ShellReply {
            output: vec!["kept".into()],
            output_truncated: false,
            result: ShellResult::Documents { documents },
            database_after: "app".into(),
            error: None,
        }
    }

    #[test]
    fn an_oversized_done_becomes_a_limit_error_that_keeps_the_output() {
        let big = serde_json::value::RawValue::from_string(format!("\"{}\"", "x".repeat(MAX_FRAME_BYTES as usize))).unwrap();
        let fitted = fit_reply(reply(vec![big]));
        let error = fitted.error.clone().expect("limit error");
        assert_eq!(error.kind, FailureKind::Limit);
        assert!(error.message.contains("64 MiB"));
        assert_eq!(fitted.output, vec!["kept"]);
        assert_eq!(fitted.database_after, "app");
        assert!(matches!(fitted.result, ShellResult::None));
        let mut frame = Vec::new();
        write_frame(&mut frame, &FromChild::Done(fitted)).expect("the replacement fits a frame");
    }

    #[test]
    fn a_reply_that_fits_is_untouched() {
        let small = serde_json::value::RawValue::from_string("{\"a\":1}".into()).unwrap();
        let fitted = fit_reply(reply(vec![small]));
        assert!(fitted.error.is_none());
        assert!(matches!(fitted.result, ShellResult::Documents { documents } if documents.len() == 1));
    }
}
