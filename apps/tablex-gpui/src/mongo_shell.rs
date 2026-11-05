//! The app's side of a MongoDB Shell run that needs no window: what a tab keeps of the last
//! run ([`ShellView`]) and the check every reply of the plugin goes through ([`check_reply`]).
//!
//! The plugin is a separate executable with no sandbox beyond what the host gives it, and a
//! script can bend its own shell (it can replace the methods that produce a result page). So the
//! reply is untrusted input here: every document must be an object and is bounded in count and
//! size, the database it says the script ended in must be a legal name, and the cursor it offers
//! for "More" must name the run's own database and well-formed ids. What is wrong is dropped or
//! turned into an error, never displayed as if the server had said it.

use engine_driver_api::mongo_shell::{
    CursorRef, FailureKind, RawDocument, ShellFailure, ShellReply, ShellResult,
};
use gpui::SharedString;
use std::borrow::Cow;
use std::fmt;

/// Documents of a result the grid shows; more are cut, with a note.
pub const MAX_RESULT_DOCUMENTS: usize = 10_000;
/// Extended JSON bytes of a result the grid shows.
pub const MAX_RESULT_BYTES: usize = 32 * 1024 * 1024;
/// Lines of `print` output kept (the plugin's own cap is the same).
pub const MAX_OUTPUT_LINES: usize = 10_000;
/// Bytes of `print` output (and of a shell text result) kept.
pub const MAX_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
/// Bytes of one line a pane draws; the line is kept whole (Copy has it) up to the caps above.
pub const MAX_DISPLAY_LINE_BYTES: usize = 4 * 1024;
/// Bytes of an error message or code name shown (the rest is cut, with a note).
pub const MAX_MESSAGE_BYTES: usize = 8 * 1024;
/// Most documents a "More" asks the server for at once.
pub const MAX_BATCH_SIZE: u32 = 1_000;
/// MongoDB's own limit on a database name, in bytes.
const MAX_DATABASE_BYTES: usize = 64;
/// MongoDB's own limit on a collection name, in bytes.
const MAX_COLLECTION_BYTES: usize = 255;

/// A server cursor the last run left open, tied to the connection (and its connect attempt) it
/// was opened on: "More" is only offered to that same session.
#[derive(Debug, Clone, PartialEq)]
pub struct MoreCursor {
    pub cursor: CursorRef,
    pub connection: String,
    /// The connection's generation when the cursor was opened: a reconnect makes it unusable.
    pub epoch: u64,
}

/// What a tab keeps of its last shell run, next to its grid.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ShellView {
    pub script: String,
    /// What the script printed, one entry per display line (an entry that held newlines is split).
    pub output: Vec<SharedString>,
    pub output_truncated: bool,
    /// A scalar or write result, as shell text (shown instead of a grid).
    pub value_text: Option<String>,
    /// `value_text` split into the lines the pane lists.
    pub value_lines: Vec<SharedString>,
    /// Documents in the grid.
    pub documents: usize,
    pub more: Option<MoreCursor>,
    pub error: Option<ShellFailure>,
    /// The database the script ended in.
    pub database: String,
    /// Things the user should know about what is shown (a cut result).
    pub notes: Vec<String>,
    /// The Output pane is open.
    pub output_open: bool,
}

impl ShellView {
    /// The Output as text (what Copy writes).
    pub fn output_text(&self) -> String {
        self.output.iter().map(SharedString::as_ref).collect::<Vec<&str>>().join("\n")
    }
}

/// A reply that passed [`check_reply`].
#[derive(Debug)]
pub struct CheckedReply {
    pub output: Vec<SharedString>,
    pub output_truncated: bool,
    pub value_text: Option<String>,
    pub value_lines: Vec<SharedString>,
    pub documents: Vec<bson::Document>,
    pub more: Option<CursorRef>,
    pub error: Option<ShellFailure>,
    /// `None` when the plugin named no legal database (the tab keeps its own).
    pub database_after: Option<String>,
    pub notes: Vec<String>,
}

/// Tests: which thread ran each step of making a reply (the UI thread must run none of them).
#[cfg(test)]
pub static THREADS: std::sync::Mutex<Vec<(&'static str, std::thread::ThreadId)>> = std::sync::Mutex::new(Vec::new());

/// Why a result document could not be taken.
#[derive(Debug)]
pub enum ReplyError {
    /// The text is not JSON.
    Json(serde_json::Error),
    /// JSON, but not an object.
    Scalar,
    ExtendedJson(bson::extjson::de::Error),
}

impl fmt::Display for ReplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(error) => write!(f, "the plugin returned a document that is not JSON: {error}"),
            Self::Scalar => f.write_str("the plugin returned a result document that is not an object"),
            Self::ExtendedJson(error) => {
                write!(f, "the plugin returned a document that is not Extended JSON: {error}")
            }
        }
    }
}

impl std::error::Error for ReplyError {}

/// Checks `reply` of a run that started in `database` on one connection. A result that cannot
/// be trusted becomes an `internal` error (with the output the script did print kept). Every
/// text that came from the plugin is cleaned (no control or bidi-override characters) and
/// bounded before it is shown.
pub fn check_reply(reply: ShellReply, database: &str) -> CheckedReply {
    #[cfg(test)]
    THREADS.lock().unwrap_or_else(|e| e.into_inner()).push(("check_reply", std::thread::current().id()));
    let mut notes = Vec::new();
    let (output, cut) = display_lines(reply.output, MAX_OUTPUT_LINES, MAX_OUTPUT_BYTES);
    let output_truncated = reply.output_truncated || cut;
    let database_after = legal_database_name(&reply.database_after).then_some(reply.database_after);
    if database_after.is_none() {
        notes.push("The plugin named a database that is not a legal name; the tab keeps its own.".into());
    }
    let error = reply.error.map(|mut failure| {
        failure.message = bounded_message(&failure.message);
        failure.code_name = failure.code_name.map(|name| bounded_message(&name));
        failure
    });
    let mut checked = CheckedReply {
        output,
        output_truncated,
        value_text: None,
        value_lines: Vec::new(),
        documents: Vec::new(),
        more: None,
        error,
        database_after,
        notes,
    };
    let current = checked.database_after.as_deref().unwrap_or(database).to_string();
    let outcome = match reply.result {
        ShellResult::None => Ok(()),
        ShellResult::Value { text } => {
            let (lines, cut) = display_lines(vec![text], MAX_OUTPUT_LINES, MAX_OUTPUT_BYTES);
            if cut {
                checked.notes.push("The text result was cut (4 MiB or 10,000 lines).".into());
            }
            checked.value_text = Some(lines.iter().map(SharedString::as_ref).collect::<Vec<&str>>().join("\n"));
            checked.value_lines = lines;
            Ok(())
        }
        ShellResult::Documents { documents } => documents_of(documents, &mut checked),
        ShellResult::Cursor { documents, cursor } => documents_of(documents, &mut checked).map(|()| {
            checked.more = cursor.and_then(|cursor| legal_cursor(cursor, database, &current));
        }),
    };
    if let Err(error) = outcome {
        checked.documents.clear();
        checked.more = None;
        checked.error.get_or_insert(ShellFailure {
            kind: FailureKind::Internal,
            message: bounded_message(&error.to_string()),
            line: None,
            column: None,
            code: None,
            code_name: None,
        });
    }
    checked
}

/// Parses the result documents into `checked`, cutting at the caps.
fn documents_of(documents: Vec<RawDocument>, checked: &mut CheckedReply) -> Result<(), ReplyError> {
    let total = documents.len();
    let mut bytes = 0usize;
    for raw in documents {
        bytes = bytes.saturating_add(raw.get().len());
        if checked.documents.len() >= MAX_RESULT_DOCUMENTS || bytes > MAX_RESULT_BYTES {
            checked.notes.push(format!(
                "Showing the first {} of {total} documents.",
                checked.documents.len()
            ));
            break;
        }
        let value: serde_json::Value = serde_json::from_str(raw.get()).map_err(ReplyError::Json)?;
        if !value.is_object() {
            return Err(ReplyError::Scalar);
        }
        match bson::Bson::try_from(value) {
            Ok(bson::Bson::Document(document)) => checked.documents.push(document),
            Ok(_) => return Err(ReplyError::Scalar),
            Err(error) => return Err(ReplyError::ExtendedJson(error)),
        }
    }
    Ok(())
}

/// `text` with the characters that must not reach a pane replaced by visible placeholders: C0
/// controls (except tab and newline; `\r` too) by their Control Pictures (`␀`..`␟`, DEL `␡`), C1
/// controls, the bidi marks / overrides / isolates (U+061C, U+200E / 200F, U+202A-202E,
/// U+2066-2069) and the line / paragraph separators (U+2028 / 2029) by U+FFFD.
pub fn clean_text(text: &str) -> Cow<'_, str> {
    let unsafe_char = |c: char| {
        matches!(c, '\u{0}'..='\u{8}' | '\u{b}'..='\u{1f}' | '\u{7f}'..='\u{9f}') || is_invisible_format(c)
    };
    if !text.chars().any(unsafe_char) {
        return Cow::Borrowed(text);
    }
    Cow::Owned(
        text.chars()
            .map(|c| match c {
                '\u{0}'..='\u{8}' | '\u{b}'..='\u{1f}' => char::from_u32(0x2400 + c as u32).unwrap_or('\u{fffd}'),
                '\u{7f}' => '\u{2421}',
                c if unsafe_char(c) => '\u{fffd}',
                c => c,
            })
            .collect(),
    )
}

/// Cleaned and cut at [`MAX_MESSAGE_BYTES`] (with a note) so a huge `throw` cannot hang a pane.
fn bounded_message(text: &str) -> String {
    let clean = clean_text(text);
    if clean.len() <= MAX_MESSAGE_BYTES {
        return clean.into_owned();
    }
    format!("{} \u{2026} (cut at 8 KiB)", &clean[..char_boundary(&clean, MAX_MESSAGE_BYTES)])
}

/// The largest index `<= at` that is a char boundary of `text`.
fn char_boundary(text: &str, at: usize) -> usize {
    let mut end = at.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

/// `line` as a pane draws it: cut at [`MAX_DISPLAY_LINE_BYTES`] with a marker.
pub fn clip_line(line: &str) -> Cow<'_, str> {
    if line.len() <= MAX_DISPLAY_LINE_BYTES {
        return Cow::Borrowed(line);
    }
    Cow::Owned(format!("{} \u{2026} (line cut here, Copy has all of it)", &line[..char_boundary(line, MAX_DISPLAY_LINE_BYTES)]))
}

/// The display lines of `entries` (each split at its newlines, `\r\n` too, then cleaned), with the
/// first lines that fit `max_lines` lines and `max_bytes` bytes in total; `true` when some did not.
fn display_lines(entries: Vec<String>, max_lines: usize, max_bytes: usize) -> (Vec<SharedString>, bool) {
    let mut lines: Vec<SharedString> = Vec::new();
    let mut bytes = 0usize;
    for entry in &entries {
        // Split first (a `\r\n` is a line end), then clean what is left of each line.
        let mut pieces: Vec<&str> = entry.split('\n').map(|line| line.strip_suffix('\r').unwrap_or(line)).collect();
        // `a\n` is one line, not `a` and an empty one; an empty entry is one empty line.
        if pieces.len() > 1 && pieces.last() == Some(&"") {
            pieces.pop();
        }
        for piece in pieces {
            let line = clean_text(piece).into_owned();
            bytes = bytes.saturating_add(line.len() + 1);
            if lines.len() >= max_lines || bytes > max_bytes {
                return (lines, true);
            }
            lines.push(SharedString::from(line));
        }
    }
    (lines, false)
}

/// MongoDB's rules, a little stricter for what a name may contain: no NUL, no `/`, `\`, `.`,
/// `"`, `$`, space, control or invisible direction character (bidi marks, overrides, isolates,
/// line / paragraph separators), 1 to 64 bytes.
pub fn legal_database_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_DATABASE_BYTES
        && !name.chars().any(|c| {
            c.is_control()
                || is_invisible_format(c)
                || matches!(c, '/' | '\\' | '.' | '"' | '$' | ' ' | '\0')
        })
}

/// Direction marks and overrides and the line / paragraph separators: U+061C, U+200E / 200F,
/// U+2028 / 2029, U+202A-202E, U+2066-2069.
fn is_invisible_format(c: char) -> bool {
    matches!(c, '\u{61c}' | '\u{200e}' | '\u{200f}' | '\u{2028}' | '\u{2029}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

fn legal_collection_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_COLLECTION_BYTES
        && !name.contains('\0')
        && !name.starts_with("system.")
        && !name.contains('$')
}

/// A cursor "More" may continue: a real id, a legal collection, and the database the run started
/// in or ended in (never another one), with a bounded batch size. The connection is not in the
/// reference at all: it is the tab's, fixed by the app when the reply arrives.
fn legal_cursor(cursor: CursorRef, started_in: &str, ended_in: &str) -> Option<CursorRef> {
    let id_ok = !cursor.id.is_empty()
        && cursor.id.len() <= 19
        && cursor.id.bytes().all(|b| b.is_ascii_digit())
        && cursor.id.parse::<i64>().is_ok_and(|id| id > 0);
    let database_ok = legal_database_name(&cursor.database)
        && (cursor.database == started_in || cursor.database == ended_in);
    (id_ok && database_ok && legal_collection_name(&cursor.collection)).then(|| CursorRef {
        batch_size: cursor.batch_size.clamp(1, MAX_BATCH_SIZE),
        ..cursor
    })
}

/// `message`, with where in the script it happened and the server's code: the line the tab shows
/// inline and the Error Log keeps.
pub fn failure_text(failure: &ShellFailure) -> String {
    let place = match (failure.line, failure.column) {
        (Some(line), Some(column)) => format!(" (line {line}, column {column})"),
        (Some(line), None) => format!(" (line {line})"),
        _ => String::new(),
    };
    let code = match (&failure.code_name, failure.code) {
        (Some(name), Some(code)) => format!(" [{name} {code}]"),
        (None, Some(code)) => format!(" [code {code}]"),
        _ => String::new(),
    };
    format!("{}{place}{code}", failure.message)
}

/// The short label of an error's kind for the tab's status line.
pub fn failure_label(kind: FailureKind) -> &'static str {
    match kind {
        FailureKind::BadRequest => "Bad request",
        FailureKind::Syntax => "Syntax error",
        FailureKind::Script => "Script error",
        FailureKind::Server => "Server error",
        FailureKind::ReadOnly => "Blocked (read-only)",
        FailureKind::Unsupported => "Not supported",
        FailureKind::Cancelled => "Cancelled",
        FailureKind::Timeout => "Timed out",
        FailureKind::Memory => "Out of memory",
        FailureKind::Limit => "Limit exceeded",
        FailureKind::Internal => "Internal error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::value::RawValue;

    fn raw(text: &str) -> Box<RawValue> {
        RawValue::from_string(text.into()).unwrap()
    }

    fn texts(lines: &[SharedString]) -> Vec<&str> {
        lines.iter().map(SharedString::as_ref).collect()
    }

    fn reply(result: ShellResult) -> ShellReply {
        ShellReply {
            output: vec!["hello".into()],
            output_truncated: false,
            result,
            database_after: "app".into(),
            error: None,
        }
    }

    fn cursor(id: &str, database: &str, collection: &str) -> CursorRef {
        CursorRef { id: id.into(), database: database.into(), collection: collection.into(), batch_size: 50 }
    }

    #[test]
    fn documents_become_bson_with_their_types() {
        let checked = check_reply(
            reply(ShellResult::Documents {
                documents: vec![
                    raw(r#"{"_id":{"$oid":"507f1f77bcf86cd799439011"},"n":{"$numberLong":"5"},"s":"x"}"#),
                    raw(r#"{"a":1}"#),
                ],
            }),
            "app",
        );
        assert!(checked.error.is_none(), "{:?}", checked.error);
        assert_eq!(checked.documents.len(), 2);
        assert!(matches!(checked.documents[0].get("_id"), Some(bson::Bson::ObjectId(_))));
        assert!(matches!(checked.documents[0].get("n"), Some(bson::Bson::Int64(5))));
        assert_eq!(checked.database_after.as_deref(), Some("app"));
        assert_eq!(texts(&checked.output), ["hello"]);
    }

    #[test]
    fn a_result_document_that_is_not_an_object_is_an_error_not_a_row() {
        for bad in ["1", r#""text""#, "[1,2]", "null"] {
            let checked = check_reply(reply(ShellResult::Documents { documents: vec![raw(bad)] }), "app");
            let error = checked.error.expect(bad);
            assert_eq!(error.kind, FailureKind::Internal, "{bad}");
            assert!(checked.documents.is_empty() && checked.more.is_none());
            assert_eq!(texts(&checked.output), ["hello"], "what was printed stays");
        }
        // A real error of the script is not replaced by the check's.
        let mut failing = reply(ShellResult::Documents { documents: vec![raw("1")] });
        failing.error = Some(ShellFailure {
            kind: FailureKind::Script, message: "boom".into(), line: Some(2), column: Some(3), code: None, code_name: None,
        });
        assert_eq!(check_reply(failing, "app").error.unwrap().message, "boom");
    }

    #[test]
    fn counts_and_sizes_are_capped() {
        let many: Vec<_> = (0..MAX_RESULT_DOCUMENTS + 5).map(|_| raw(r#"{"a":1}"#)).collect();
        let checked = check_reply(reply(ShellResult::Documents { documents: many }), "app");
        assert_eq!(checked.documents.len(), MAX_RESULT_DOCUMENTS);
        assert!(checked.notes.iter().any(|note| note.contains("first 10000")), "{:?}", checked.notes);

        let lines: Vec<String> = (0..MAX_OUTPUT_LINES + 3).map(|n| n.to_string()).collect();
        let mut loud = reply(ShellResult::None);
        loud.output = lines;
        let checked = check_reply(loud, "app");
        assert_eq!(checked.output.len(), MAX_OUTPUT_LINES);
        assert!(checked.output_truncated);

        let mut wide = reply(ShellResult::None);
        wide.output = vec!["x".repeat(MAX_OUTPUT_BYTES), "y".into()];
        assert!(check_reply(wide, "app").output.is_empty(), "a line past the byte budget is cut");

        let text = check_reply(reply(ShellResult::Value { text: "é".repeat(MAX_OUTPUT_BYTES) }), "app");
        assert!(text.value_text.unwrap().len() <= MAX_OUTPUT_BYTES);
        assert!(!text.notes.is_empty());
    }

    #[test]
    fn database_names_are_checked() {
        for good in ["app", "my-db", "db_1", "Ünï", &"a".repeat(64)] {
            assert!(legal_database_name(good), "{good}");
        }
        for invisible in ['\u{200e}', '\u{200f}', '\u{61c}', '\u{2028}', '\u{2029}', '\u{202a}', '\u{202e}', '\u{2066}', '\u{2069}'] {
            assert!(!legal_database_name(&format!("a{invisible}b")), "{invisible:?}");
        }
        for bad in ["", "a/b", "a\\b", "a.b", "a b", "a\"b", "a$b", "nul\0", "tab\t", &"a".repeat(65)] {
            assert!(!legal_database_name(bad), "{bad:?}");
        }
        let mut odd = reply(ShellResult::None);
        odd.database_after = "../etc".into();
        let checked = check_reply(odd, "app");
        assert_eq!(checked.database_after, None, "the tab keeps its own database");
        assert!(!checked.notes.is_empty());
    }

    #[test]
    fn a_cursor_may_only_continue_on_the_runs_own_database() {
        let page = |reference| {
            check_reply(
                reply(ShellResult::Cursor { documents: vec![raw(r#"{"a":1}"#)], cursor: Some(reference) }),
                "app",
            )
            .more
        };
        assert_eq!(page(cursor("4242", "app", "items")).unwrap().id, "4242");
        for bad in [
            cursor("4242", "other", "items"),
            cursor("0", "app", "items"),
            cursor("-5", "app", "items"),
            cursor("12ab", "app", "items"),
            cursor("99999999999999999999", "app", "items"),
            cursor("4242", "app", ""),
            cursor("4242", "app", "system.users"),
            cursor("4242", "a/b", "items"),
        ] {
            assert!(page(bad.clone()).is_none(), "{bad:?}");
        }
        let mut huge = cursor("7", "app", "items");
        huge.batch_size = 4_000_000_000;
        assert_eq!(page(huge).unwrap().batch_size, MAX_BATCH_SIZE);
        // After `use other`, the cursor of the database the script ended in is fine too.
        let mut moved = reply(ShellResult::Cursor { documents: vec![], cursor: Some(cursor("9", "other", "c")) });
        moved.database_after = "other".into();
        assert!(check_reply(moved, "app").more.is_some());
    }

    #[test]
    fn failures_read_with_their_place_and_code() {
        let failure = ShellFailure {
            kind: FailureKind::Server,
            message: "duplicate key".into(),
            line: Some(3),
            column: Some(9),
            code: Some(11000),
            code_name: Some("DuplicateKey".into()),
        };
        assert_eq!(failure_text(&failure), "duplicate key (line 3, column 9) [DuplicateKey 11000]");
        let plain = ShellFailure { kind: FailureKind::Script, message: "x".into(), line: None, column: None, code: None, code_name: None };
        assert_eq!(failure_text(&plain), "x");
    }

    #[test]
    fn a_print_with_newlines_is_several_display_lines() {
        // `printjson` is pretty: one entry holds a whole block.
        let block = "{\n  \"a\": 1,\r\n  \"b\": [\n    2\n  ]\n}".to_string();
        let mut printing = reply(ShellResult::None);
        printing.output = vec!["before".into(), block, "".into(), "tail\n".into()];
        let checked = check_reply(printing, "app");
        assert_eq!(
            texts(&checked.output),
            ["before", "{", "  \"a\": 1,", "  \"b\": [", "    2", "  ]", "}", "", "tail"]
        );
        // The text result is split the same way and Copy has it whole.
        let value = check_reply(reply(ShellResult::Value { text: "{\n  ok: 1\n}".into() }), "app");
        assert_eq!(texts(&value.value_lines), ["{", "  ok: 1", "}"]);
        assert_eq!(value.value_text.as_deref(), Some("{\n  ok: 1\n}"));
        // The caps count display lines: 3 entries of 4,000 lines are cut at 10,000 lines.
        let many = "x\n".repeat(4_000);
        let mut loud = reply(ShellResult::None);
        loud.output = vec![many.clone(), many.clone(), many];
        let checked = check_reply(loud, "app");
        assert_eq!(checked.output.len(), MAX_OUTPUT_LINES);
        assert!(checked.output_truncated);
    }

    #[test]
    fn an_error_message_is_bounded_and_clean() {
        let huge = "x".repeat(30_000_000);
        let mut failing = reply(ShellResult::None);
        failing.error = Some(ShellFailure {
            kind: FailureKind::Script,
            message: huge,
            line: Some(1),
            column: Some(1),
            code: None,
            code_name: Some("N".repeat(100_000)),
        });
        let error = check_reply(failing, "app").error.unwrap();
        assert!(error.message.len() < MAX_MESSAGE_BYTES + 64 && error.message.ends_with("(cut at 8 KiB)"), "{}", error.message.len());
        assert!(error.code_name.unwrap().len() < MAX_MESSAGE_BYTES + 64);
        // A cut never splits a character.
        let mut multibyte = reply(ShellResult::None);
        multibyte.error = Some(ShellFailure { kind: FailureKind::Script, message: "é".repeat(20_000), line: None, column: None, code: None, code_name: None });
        assert!(check_reply(multibyte, "app").error.unwrap().message.starts_with("éé"));
    }

    #[test]
    fn controls_and_bidi_overrides_become_visible_placeholders() {
        assert_eq!(clean_text("plain\ttext\n"), "plain\ttext\n");
        assert_eq!(clean_text("a\u{1b}[31mb\0c\u{7f}d"), "a\u{241b}[31mb\u{2400}c\u{2421}d");
        assert_eq!(clean_text("x\u{85}y\u{9f}z"), "x\u{fffd}y\u{fffd}z");
        for bidi in ['\u{202a}', '\u{202e}', '\u{2066}', '\u{2069}', '\u{200e}', '\u{200f}', '\u{61c}', '\u{2028}', '\u{2029}'] {
            assert_eq!(clean_text(&format!("a{bidi}b")), "a\u{fffd}b", "{bidi:?}");
        }
        let mut dirty = reply(ShellResult::None);
        dirty.output = vec!["safe\u{1b}]0;title\u{7}\u{202e}txt".into()];
        dirty.error = Some(ShellFailure { kind: FailureKind::Script, message: "bad\u{1b}[2J".into(), line: None, column: None, code: None, code_name: Some("N\u{202e}".into()) });
        let checked = check_reply(dirty, "app");
        assert!(texts(&checked.output)[0].chars().all(|c| !c.is_control() && c != '\u{202e}'));
        let error = checked.error.unwrap();
        assert!(!error.message.contains('\u{1b}') && !error.code_name.unwrap().contains('\u{202e}'));
    }

    #[test]
    fn a_long_line_is_clipped_for_drawing_but_kept_whole_for_copy() {
        let long = "y".repeat(MAX_DISPLAY_LINE_BYTES * 3);
        let clipped = clip_line(&long);
        assert!(clipped.len() < MAX_DISPLAY_LINE_BYTES + 80 && clipped.contains("Copy has all"));
        assert_eq!(clip_line("short"), "short");
        let mut printing = reply(ShellResult::None);
        printing.output = vec![long.clone()];
        let checked = check_reply(printing, "app");
        assert_eq!(checked.output[0].len(), long.len(), "the entry is whole");
    }
}
