//! Wire contract of the optional MongoDB Shell plugin (`tablex-mongo-shell`).
//!
//! The plugin is a separate **executable** the host spawns once per script run
//! (never code loaded into the app): it runs mongosh-style scripts in an
//! embedded JavaScript engine and owns no MongoDB client. Every server call
//! goes back to the host as a frame, which the host wires to the driver's
//! `mongo_command` op (capability `mongo.command`).
//!
//! # Protocol (version [`PROTOCOL_VERSION`])
//!
//! Frames travel over the child's stdin (host to child, [`ToChild`]) and stdout
//! (child to host, [`FromChild`]); stderr is diagnostics only. A frame is a
//! 4-byte big-endian length followed by that many bytes of UTF-8 JSON; a frame
//! larger than [`MAX_FRAME_BYTES`] is refused in both directions
//! ([`read_frame`] / [`write_frame`]).
//!
//! ```text
//! child -> host   {"hello":{"protocol":1}}                     first frame; a mismatch ends the run
//! host  -> child  {"run":{script,database,allow_writes,page_size,timeout_ms?,cursor?}}
//! child -> host   {"command":{"id":1,"database":"app","command":{...EJSON...}}}
//! host  -> child  {"reply":{"id":1,"ok":{...EJSON...}}}        or {"reply":{"id":1,"error":{kind,code?,message}}}
//! ...             any number of command / reply pairs, strictly one at a time
//! child -> host   {"done":{output,result,database_after,error?}}   last frame, then the child exits
//! ```
//!
//! A malformed or oversized frame, a reply with an unknown id, or a child that
//! exits without `done` ends the run with a typed error and the host kills the
//! child. Stop and the wall-clock timeout are the host killing the child.
//! Commands and replies are Extended JSON v2 with object keys in order and every
//! BSON type tagged, so no value loses its type; the one liberty is that an
//! Int32 may be a plain JSON integer (a plain integer is always an Int32 here).
//! Server replies are canonical; documents the script itself produced (a
//! document result) may use the plain form for Int32.

use serde::{Deserialize, Serialize};
use std::io::{Read, Write};

/// Version of the protocol above; bumped on any incompatible change.
pub const PROTOCOL_VERSION: u32 = 1;

/// Largest frame either side accepts (64 MiB).
pub const MAX_FRAME_BYTES: u64 = 64 * 1024 * 1024;

/// Host to child.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToChild {
    Run(ShellRequest),
    Reply(CommandReply),
}

/// Child to host.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FromChild {
    Hello { protocol: u32 },
    Command(CommandCall),
    Done(ShellReply),
}

/// One server command the script issued.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandCall {
    pub id: u64,
    pub database: String,
    /// Canonical Extended JSON; the first key is the command name.
    pub command: RawDocument,
}

/// The host's answer to one [`CommandCall`]: exactly one of `ok` / `error`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandReply {
    pub id: u64,
    /// The server's reply document (canonical Extended JSON).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ok: Option<RawDocument>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<CommandFailure>,
}

/// Why a command produced no reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandFailure {
    pub kind: CommandFailureKind,
    /// The driver's error code when it refused (`query.read_only_violation`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandFailureKind {
    /// Transport or server trouble.
    Failed,
    Cancelled,
    /// The driver refused the request (read-only guard, bad request).
    Refused,
}

/// Why a frame could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("pipe error: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame of {0} bytes exceeds the {MAX_FRAME_BYTES}-byte limit")]
    TooLarge(u64),
    #[error("the stream ended inside a frame")]
    Truncated,
    #[error("malformed frame: {0}")]
    Malformed(String),
}

/// Writes one frame and flushes.
pub fn write_frame<T: Serialize>(writer: &mut impl Write, frame: &T) -> Result<(), FrameError> {
    let bytes = serde_json::to_vec(frame).map_err(|error| FrameError::Malformed(error.to_string()))?;
    let length = bytes.len() as u64;
    if length > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(length));
    }
    writer.write_all(&(length as u32).to_be_bytes())?;
    writer.write_all(&bytes)?;
    writer.flush()?;
    Ok(())
}

/// Reads one frame; `None` when the stream ended cleanly between frames. The
/// declared length is checked before anything is allocated.
pub fn read_frame<T: for<'de> Deserialize<'de>>(
    reader: &mut impl Read,
) -> Result<Option<T>, FrameError> {
    let mut header = [0u8; 4];
    let mut filled = 0;
    while filled < header.len() {
        match reader.read(&mut header[filled..]) {
            Ok(0) if filled == 0 => return Ok(None),
            Ok(0) => return Err(FrameError::Truncated),
            Ok(count) => filled += count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
    let length = u64::from(u32::from_be_bytes(header));
    if length > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(length));
    }
    let mut body = vec![0u8; length as usize];
    reader.read_exact(&mut body).map_err(|error| match error.kind() {
        std::io::ErrorKind::UnexpectedEof => FrameError::Truncated,
        _ => FrameError::Io(error),
    })?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|error| FrameError::Malformed(error.to_string()))
}

/// One script run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShellRequest {
    pub script: String,
    /// Database the script starts in (`db`).
    pub database: String,
    /// `false` refuses everything that can write before it reaches the server.
    #[serde(default)]
    pub allow_writes: bool,
    /// Documents a cursor result returns in its first page.
    #[serde(default = "default_page_size")]
    pub page_size: u32,
    /// Wall-clock limit of the whole run; none means only a cancel stops it.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// The cursor the previous reply left open (mongosh `it` continues it).
    #[serde(default)]
    pub cursor: Option<CursorRef>,
}

fn default_page_size() -> u32 {
    DEFAULT_PAGE_SIZE
}

/// Default [`ShellRequest::page_size`].
pub const DEFAULT_PAGE_SIZE: u32 = 50;

/// A server cursor that has more documents: continue it with
/// `{ getMore: <id>, collection: <collection>, batchSize }` in `database`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CursorRef {
    /// The server cursor id, a decimal string (an Int64 does not fit a JSON number).
    pub id: String,
    pub database: String,
    pub collection: String,
    pub batch_size: u32,
}

/// One document as canonical Extended JSON text, kept verbatim so the keys
/// stay in the server's order.
pub type RawDocument = Box<serde_json::value::RawValue>;

/// What the script's last statement produced.
///
/// On the wire: `{"kind": "none"}` or `{"kind": ..., "data": {...}}` (adjacently
/// tagged so the documents stay verbatim text).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum ShellResult {
    /// Nothing to show (the last statement had no value).
    None,
    /// A scalar or a write result, as shell text.
    Value { text: String },
    /// A document or an array of documents (canonical Extended JSON, keys in order).
    Documents { documents: Vec<RawDocument> },
    /// The first page of a cursor; `cursor` is set while the server has more.
    Cursor {
        documents: Vec<RawDocument>,
        cursor: Option<CursorRef>,
    },
}

/// Why a run failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    /// The request itself was malformed.
    BadRequest,
    /// The script does not parse.
    Syntax,
    /// A JavaScript exception the script did not handle.
    Script,
    /// The server (or driver) rejected a command.
    Server,
    /// A write was refused because the run is read-only.
    ReadOnly,
    /// Uses something the shell does not provide (`load`, `require`, ...).
    Unsupported,
    Cancelled,
    Timeout,
    /// The script used more than the memory limit.
    Memory,
    /// A size limit (output, `toArray`) was exceeded.
    Limit,
    Internal,
}

/// An error with where it happened.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShellFailure {
    pub kind: FailureKind,
    pub message: String,
    /// 1-based position in the script, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,
    /// The server's error code and name, for [`FailureKind::Server`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_name: Option<String>,
}

/// The reply of one run. `output` and `database_after` are filled even when
/// the run failed (what was printed before the error).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellReply {
    pub output: Vec<String>,
    /// Output hit the line / byte cap; the last line says so.
    #[serde(default)]
    pub output_truncated: bool,
    pub result: ShellResult,
    /// The database `db` points at when the run ended (after `use`).
    pub database_after: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ShellFailure>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded(frame: &impl Serialize) -> Vec<u8> {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, frame).unwrap();
        bytes
    }

    #[test]
    fn frames_round_trip_and_keep_documents_verbatim() {
        let call = FromChild::Command(CommandCall {
            id: 7,
            database: "app".into(),
            command: serde_json::value::RawValue::from_string(r#"{"find":"c","z":1,"a":{"$numberLong":"5"}}"#.into()).unwrap(),
        });
        let bytes = encoded(&call);
        assert_eq!(u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize, bytes.len() - 4);
        let FromChild::Command(back) = read_frame::<FromChild>(&mut &bytes[..]).unwrap().unwrap() else {
            panic!("not a command")
        };
        assert_eq!((back.id, back.database.as_str()), (7, "app"));
        assert_eq!(back.command.get(), r#"{"find":"c","z":1,"a":{"$numberLong":"5"}}"#, "key order intact");
    }

    #[test]
    fn frames_follow_each_other_and_a_clean_end_is_none() {
        let mut stream = encoded(&FromChild::Hello { protocol: PROTOCOL_VERSION });
        stream.extend(encoded(&ToChild::Reply(CommandReply { id: 1, ok: None, error: None })));
        let mut reader = &stream[..];
        assert!(matches!(read_frame::<FromChild>(&mut reader).unwrap(), Some(FromChild::Hello { protocol: 1 })));
        assert!(matches!(read_frame::<ToChild>(&mut reader).unwrap(), Some(ToChild::Reply(_))));
        assert!(read_frame::<ToChild>(&mut reader).unwrap().is_none());
    }

    #[test]
    fn the_wire_shape_is_what_the_docs_say() {
        let hello = String::from_utf8(encoded(&FromChild::Hello { protocol: 1 })[4..].to_vec()).unwrap();
        assert_eq!(hello, r#"{"hello":{"protocol":1}}"#);
        let run = ToChild::Run(ShellRequest {
            script: "1".into(),
            database: "d".into(),
            allow_writes: false,
            page_size: 50,
            timeout_ms: None,
            cursor: None,
        });
        let text = String::from_utf8(encoded(&run)[4..].to_vec()).unwrap();
        assert!(text.starts_with(r#"{"run":{"script":"1","database":"d","allow_writes":false,"page_size":50"#), "{text}");
        let none = serde_json::to_string(&ShellResult::None).unwrap();
        assert_eq!(none, r#"{"kind":"none"}"#);
    }

    #[test]
    fn oversized_declared_lengths_are_refused_before_allocation() {
        let mut bytes = (MAX_FRAME_BYTES as u32 + 1).to_be_bytes().to_vec();
        bytes.extend_from_slice(b"x");
        assert!(matches!(read_frame::<FromChild>(&mut &bytes[..]), Err(FrameError::TooLarge(_))));
        let huge = u32::MAX.to_be_bytes();
        assert!(matches!(read_frame::<FromChild>(&mut &huge[..]), Err(FrameError::TooLarge(_))));
    }

    #[test]
    fn oversized_frames_are_refused_when_writing_too() {
        let big = serde_json::value::RawValue::from_string(format!("\"{}\"", "x".repeat(MAX_FRAME_BYTES as usize))).unwrap();
        let call = FromChild::Command(CommandCall { id: 1, database: "d".into(), command: big });
        assert!(matches!(write_frame(&mut Vec::new(), &call), Err(FrameError::TooLarge(_))));
    }

    #[test]
    fn truncated_and_malformed_frames_are_errors() {
        let bytes = encoded(&FromChild::Hello { protocol: 1 });
        assert!(matches!(read_frame::<FromChild>(&mut &bytes[..bytes.len() - 2]), Err(FrameError::Truncated)));
        assert!(matches!(read_frame::<FromChild>(&mut &bytes[..2]), Err(FrameError::Truncated)));
        let mut junk = 4u32.to_be_bytes().to_vec();
        junk.extend_from_slice(b"nope");
        assert!(matches!(read_frame::<FromChild>(&mut &junk[..]), Err(FrameError::Malformed(_))));
        let mut wrong = 2u32.to_be_bytes().to_vec();
        wrong.extend_from_slice(b"{}");
        assert!(matches!(read_frame::<FromChild>(&mut &wrong[..]), Err(FrameError::Malformed(_))));
    }
}
