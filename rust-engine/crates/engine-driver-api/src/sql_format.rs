//! Wire contract of the optional SQL formatter tool (`tablex-sqruff`).
//!
//! Like the MongoDB Shell plugin, the tool is a separate **executable** the host spawns once per
//! format (never code loaded into the app): it reads one request, lays the SQL out and answers.
//! It owns no connection and touches no file. Frames are the ones of [`crate::mongo_shell`]
//! ([`read_frame`] / [`write_frame`]: a 4-byte big-endian length, then UTF-8 JSON, at most
//! [`MAX_FRAME_BYTES`]); stderr is diagnostics only.
//!
//! # Protocol (version [`PROTOCOL_VERSION`])
//!
//! ```text
//! tool -> host   {"hello":{"protocol":1}}                                   first frame; a mismatch ends the run
//! host -> tool   {"format":{"protocol":1,"sql":...,"dialect":"postgres","keyword_case":"upper","indent":2}}
//! tool -> host   {"done":{"formatted":...}}                                  or {"failed":{"kind":...,"message":...}}
//! ```
//!
//! then the tool exits. The tool changes layout and the case of keywords only; the host still
//! checks every answer before it replaces the user's text (the app's guard).

use crate::mongo_shell::{read_frame, write_frame, FrameError};
use serde::{Deserialize, Serialize};

pub use crate::mongo_shell::MAX_FRAME_BYTES;

/// Version of the protocol above; bumped on any incompatible change.
pub const PROTOCOL_VERSION: u32 = 1;

/// The SQL dialect the text is written in (how the tool parses it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dialect {
    /// Standard SQL: for a database the tool has no dialect of.
    Ansi,
    Postgres,
    Mysql,
    Mariadb,
    Sqlite,
    Clickhouse,
}

impl Dialect {
    pub const ALL: [Self; 6] = [
        Self::Ansi,
        Self::Postgres,
        Self::Mysql,
        Self::Mariadb,
        Self::Sqlite,
        Self::Clickhouse,
    ];

    /// The wire (and sqruff / sqlfluff `--dialect`) name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ansi => "ansi",
            Self::Postgres => "postgres",
            Self::Mysql => "mysql",
            Self::Mariadb => "mariadb",
            Self::Sqlite => "sqlite",
            Self::Clickhouse => "clickhouse",
        }
    }
}

/// The case formatting gives keywords.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeywordCase {
    Upper,
    Lower,
    /// As typed.
    Keep,
}

/// Host to tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToTool {
    Format(FormatRequest),
}

/// One format.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FormatRequest {
    /// The host's [`PROTOCOL_VERSION`]; the tool refuses a request of another version.
    pub protocol: u32,
    pub sql: String,
    pub dialect: Dialect,
    pub keyword_case: KeywordCase,
    /// Spaces per indent level.
    pub indent: u8,
}

/// Tool to host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FromTool {
    Hello { protocol: u32 },
    Done { formatted: String },
    Failed(Failure),
}

/// Why nothing was formatted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    pub kind: FailureKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    /// The request is malformed or of another protocol version.
    BadRequest,
    /// The SQL does not parse in the dialect: formatting it would be guessing.
    Parse,
    /// The text is larger than the tool formats.
    TooLarge,
    Internal,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Runs the tool's side of the protocol on `reader` / `writer` (its stdin / stdout): hello, one
/// request, one answer. `format` is the tool's formatter. `Ok(false)` when the host closed the
/// pipe before asking (the smoke test: hello and exit).
pub fn serve(
    reader: &mut impl std::io::Read,
    writer: &mut impl std::io::Write,
    format: impl FnOnce(&FormatRequest) -> Result<String, Failure>,
) -> Result<bool, FrameError> {
    write_frame(writer, &FromTool::Hello { protocol: PROTOCOL_VERSION })?;
    let Some(ToTool::Format(request)) = read_frame::<ToTool>(reader)? else {
        return Ok(false);
    };
    let answer = if request.protocol != PROTOCOL_VERSION {
        FromTool::Failed(Failure {
            kind: FailureKind::BadRequest,
            message: format!(
                "the request speaks protocol {}, this tool speaks {PROTOCOL_VERSION}",
                request.protocol
            ),
        })
    } else {
        match format(&request) {
            Ok(formatted) => FromTool::Done { formatted },
            Err(failure) => FromTool::Failed(failure),
        }
    };
    write_frame(writer, &answer)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(protocol: u32) -> ToTool {
        ToTool::Format(FormatRequest {
            protocol,
            sql: "select 1".into(),
            dialect: Dialect::Postgres,
            keyword_case: KeywordCase::Upper,
            indent: 2,
        })
    }

    fn framed(frame: &impl Serialize) -> Vec<u8> {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, frame).unwrap();
        bytes
    }

    fn answers(output: &[u8]) -> (FromTool, FromTool) {
        let mut reader = output;
        let hello = read_frame::<FromTool>(&mut reader).unwrap().unwrap();
        let answer = read_frame::<FromTool>(&mut reader).unwrap().unwrap();
        (hello, answer)
    }

    #[test]
    fn the_wire_shape_is_what_the_docs_say() {
        let text = |bytes: Vec<u8>| String::from_utf8(bytes[4..].to_vec()).unwrap();
        assert_eq!(text(framed(&FromTool::Hello { protocol: 1 })), r#"{"hello":{"protocol":1}}"#);
        assert_eq!(
            text(framed(&request(1))),
            r#"{"format":{"protocol":1,"sql":"select 1","dialect":"postgres","keyword_case":"upper","indent":2}}"#
        );
        assert_eq!(text(framed(&FromTool::Done { formatted: "SELECT 1".into() })), r#"{"done":{"formatted":"SELECT 1"}}"#);
        let failed = FromTool::Failed(Failure { kind: FailureKind::Parse, message: "no".into() });
        assert_eq!(text(framed(&failed)), r#"{"failed":{"kind":"parse","message":"no"}}"#);
    }

    #[test]
    fn a_request_round_trips_through_serve() {
        let mut output = Vec::new();
        let served = serve(&mut &framed(&request(PROTOCOL_VERSION))[..], &mut output, |request| {
            Ok(request.sql.to_uppercase())
        })
        .unwrap();
        assert!(served);
        let (hello, answer) = answers(&output);
        assert_eq!(hello, FromTool::Hello { protocol: PROTOCOL_VERSION });
        assert_eq!(answer, FromTool::Done { formatted: "SELECT 1".into() });
    }

    #[test]
    fn a_request_of_another_version_is_refused_without_formatting() {
        let mut output = Vec::new();
        serve(&mut &framed(&request(PROTOCOL_VERSION + 1))[..], &mut output, |_| {
            panic!("must not format")
        })
        .unwrap();
        let (_, answer) = answers(&output);
        assert!(matches!(answer, FromTool::Failed(Failure { kind: FailureKind::BadRequest, .. })));
    }

    #[test]
    fn a_closed_stdin_after_hello_is_the_smoke_test() {
        let mut output = Vec::new();
        assert!(!serve(&mut &b""[..], &mut output, |_| panic!("no request")).unwrap());
        assert_eq!(read_frame::<FromTool>(&mut &output[..]).unwrap(), Some(FromTool::Hello { protocol: PROTOCOL_VERSION }));
    }

    #[test]
    fn a_formatter_failure_is_a_typed_answer() {
        let mut output = Vec::new();
        serve(&mut &framed(&request(PROTOCOL_VERSION))[..], &mut output, |_| {
            Err(Failure { kind: FailureKind::Parse, message: "line 1".into() })
        })
        .unwrap();
        let (_, answer) = answers(&output);
        assert_eq!(answer, FromTool::Failed(Failure { kind: FailureKind::Parse, message: "line 1".into() }));
    }

    #[test]
    fn an_unknown_dialect_or_garbage_is_a_malformed_frame() {
        let mut bytes = Vec::new();
        let body = br#"{"format":{"protocol":1,"sql":"x","dialect":"cobol","keyword_case":"upper","indent":2}}"#;
        bytes.extend((body.len() as u32).to_be_bytes());
        bytes.extend(body);
        assert!(matches!(read_frame::<ToTool>(&mut &bytes[..]), Err(FrameError::Malformed(_))));
    }

    #[test]
    fn dialect_names_are_the_wire_names() {
        for dialect in Dialect::ALL {
            assert_eq!(serde_json::to_string(&dialect).unwrap(), format!("\"{}\"", dialect.as_str()));
        }
    }
}
