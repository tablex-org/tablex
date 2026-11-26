//! How many statements DuckDB itself reads in a piece of text.
//!
//! The duckdb crate's `prepare` splits its text with DuckDB's parser and RUNS every statement but
//! the last, so a text that the driver's lexer takes for one statement but DuckDB splits in two
//! would run the first one unseen: past the read-only scope, past the agent classifier. The lexer
//! mirrors DuckDB's tokenizer (`SqlLexOptions::duckdb`), and this is the check that does not rely
//! on that: the driver asks DuckDB's own `duckdb_extract_statements` and refuses a statement
//! DuckDB reads as anything but one.
//!
//! The parse needs a connection but reads no data, so it runs on a private in-memory instance of
//! its own (one thread, no file or network access), made on first use and kept for the process.
//! That instance has DuckDB's default grammar: a parser extension or parser setting loaded on the
//! user's own (unlocked) instance is not mirrored in it. A text that only such an extension reads
//! differently is a gap, accepted: the agent tools refuse every statement on an instance that is
//! not locked, and a locked one cannot load an extension.

use duckdb::ffi;
use std::ffi::{c_char, c_void, CStr, CString};
use std::ptr;
use std::sync::{Mutex, PoisonError};

struct Parser {
    connection: ffi::duckdb_connection,
}

// SAFETY: a DuckDB connection may be used from any thread, one at a time: the mutex below.
unsafe impl Send for Parser {}

/// The parse-only instance, or why it could not be made: a failed open is kept and not retried
/// (opening an instance is heavy, and it fails for reasons that do not go away within the process,
/// so every query would pay for it again to fail the same way).
static PARSER: Mutex<Option<Result<Parser, String>>> = Mutex::new(None);

/// Opens the parse-only instance. It is never closed: it lives as long as the process.
fn open() -> Result<Parser, String> {
    // SAFETY: plain C API calls on handles made here; every handle is checked and freed on failure.
    unsafe {
        let mut config: ffi::duckdb_config = ptr::null_mut();
        if ffi::duckdb_create_config(&mut config) != ffi::DuckDBSuccess {
            return Err("could not configure the statement splitter".into());
        }
        for (name, value) in [
            (c"threads", c"1"),
            (c"enable_external_access", c"false"),
            (c"autoinstall_known_extensions", c"false"),
        ] {
            ffi::duckdb_set_config(config, name.as_ptr(), value.as_ptr());
        }
        let mut database: ffi::duckdb_database = ptr::null_mut();
        let mut error: *mut c_char = ptr::null_mut();
        let state = ffi::duckdb_open_ext(ptr::null(), &mut database, config, &mut error);
        ffi::duckdb_destroy_config(&mut config);
        if state != ffi::DuckDBSuccess {
            let message = if error.is_null() {
                "could not open the statement splitter".to_owned()
            } else {
                let message = CStr::from_ptr(error).to_string_lossy().into_owned();
                ffi::duckdb_free(error.cast::<c_void>());
                message
            };
            return Err(message);
        }
        let mut connection: ffi::duckdb_connection = ptr::null_mut();
        if ffi::duckdb_connect(database, &mut connection) != ffi::DuckDBSuccess {
            ffi::duckdb_close(&mut database);
            return Err("could not connect the statement splitter".into());
        }
        Ok(Parser { connection })
    }
}

/// What DuckDB makes of a text.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Count {
    /// It reads this many statements.
    Statements(usize),
    /// It cannot parse the text (its message). `prepare` fails on the same text before it runs
    /// anything, so nothing is smuggled; the driver lets that failure happen on the user's
    /// connection, where DuckDB treats it as it always does (inside a transaction, it aborts it).
    ParseError(String),
}

/// How many statements DuckDB reads in `sql`; `Err` when the parse-only instance cannot be used,
/// which must refuse the text (it cannot be checked).
pub(crate) fn statement_count(sql: &str) -> Result<Count, String> {
    let Ok(text) = CString::new(sql) else {
        // `prepare` cannot take it either.
        return Ok(Count::ParseError("the SQL contains a NUL byte".into()));
    };
    let mut guard = PARSER.lock().unwrap_or_else(PoisonError::into_inner);
    let parser = match guard.get_or_insert_with(open) {
        Ok(parser) => &*parser,
        Err(message) => return Err(message.clone()),
    };
    // SAFETY: `connection` is a live connection; `extracted` is always set by the call and is
    // destroyed exactly once; the error text is read before that.
    unsafe {
        let mut extracted: ffi::duckdb_extracted_statements = ptr::null_mut();
        let count = ffi::duckdb_extract_statements(parser.connection, text.as_ptr(), &mut extracted);
        let error = ffi::duckdb_extract_statements_error(extracted);
        let result = if error.is_null() {
            Ok(Count::Statements(usize::try_from(count).unwrap_or(usize::MAX)))
        } else {
            Ok(Count::ParseError(CStr::from_ptr(error).to_string_lossy().into_owned()))
        };
        ffi::duckdb_destroy_extracted(&mut extracted);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::{statement_count, Count};

    #[test]
    fn duckdb_counts_the_statements_it_will_run() {
        let count = |sql: &str| statement_count(sql).unwrap();
        assert_eq!(count("SELECT 1"), Count::Statements(1));
        assert_eq!(count("SELECT 1; SELECT 2;"), Count::Statements(2));
        assert_eq!(count("SELECT 1 AS `'`; COMMIT; DELETE FROM t; SELECT 1 AS `'`"), Count::Statements(4));
        assert_eq!(count("SELECT 'a;b' -- c; d\n"), Count::Statements(1));
        assert!(matches!(count("SELEC 1"), Count::ParseError(_)));
        assert!(matches!(count("SELECT 1\0; SELECT 2"), Count::ParseError(_)));
    }
}
