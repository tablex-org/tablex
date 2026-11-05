//! SASLprep (RFC 4013, the "stringprep" profile of RFC 3454) as PostgreSQL applies it to a password before it is hashed.
//!
//! PostgreSQL's server and libpq map the password (non-ASCII spaces to a space, "commonly mapped to nothing" removed), normalize it with
//! NFKC, and check the result against the prohibited tables, the unassigned table (A.1, Unicode 3.2) and the bidirectional rule. A password
//! that fails a check is hashed **as typed**; one that passes is hashed normalized. [`stringprep::saslprep`] (the crate `tokio-postgres`'s
//! SCRAM uses, with the RFC 3454 tables) does the same, so that is what decides here: `Ok(prepared)` is hashed, an `Err` falls back to the
//! password as typed. A client that decided differently from the server would make a verifier the real password cannot log in with.

use std::borrow::Cow;

/// The text PostgreSQL hashes for `password`: the prepared text, or the password as typed where SASLprep refuses it. A `Cow::Owned` result
/// is a copy of the password this code made: the caller wipes it after hashing.
pub fn prepare(password: &str) -> Cow<'_, str> {
    stringprep::saslprep(password).unwrap_or(Cow::Borrowed(password))
}
