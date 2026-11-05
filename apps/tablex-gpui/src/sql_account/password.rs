//! A password the user typed, and what keeps it out of every visible place.
//!
//! [`Password`] is one shared cell around the engine's [`Secret`] (redacted `Debug`, zeroized on drop). Every clone of a
//! change that carries it (the draft, an undo step, the request, the apply job) points at the same cell, so wiping it once
//! ([`Password::wipe`]) wipes the password everywhere: after an Apply, and when the review is closed. A wiped password makes
//! the statement that needs it unplannable ([`super::AccountError::PasswordForgotten`]) instead of silently creating a
//! passwordless account.
//!
//! **What is wiped, and what is not.** Wiped (zeroized, by `Secret`'s drop or [`wipe_string`]): the cell of a [`Password`] (once, for every clone),
//! the executable statements of a change-set ([`super::Executable`]) and the clauses they are joined from, the needles of a [`Scrubber`],
//! the request JSON the apply hands to the driver and the statement the request holds, the prepared copy of a PostgreSQL password and the
//! PBKDF2 key. Not wiped, because the code that holds them is not ours: the text of the masked `Input` and the `SharedString`s it hands out,
//! the `String` made to fill the input again when the user comes back to an account, the internal state of the HMAC / SHA-2 / PBKDF2 crates, the
//! C string `engine-driver-host` builds from the request, and whatever the driver and the server keep. Memory freed is not memory overwritten.
//!
//! The statements the review shows, copies, logs and reports always carry [`REDACTED`] in place of the password
//! ([`Reveal::Redacted`]). The text with the real value ([`Reveal::Exec`]) is made only on the apply's own thread and goes
//! to the driver and nowhere else; [`Scrubber`] removes it from the server's error text before anything shows that.

use std::sync::{Arc, Mutex, MutexGuard};

use engine_net::Secret;

/// What a visible statement shows where the password goes.
pub const REDACTED: &str = "********";

/// The placeholder as a string literal, where a statement has the password.
pub const REDACTED_LITERAL: &str = "'********'";

/// Which text of a statement is wanted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reveal {
    /// For the review, Copy SQL, the error log: [`REDACTED`] stands for the password.
    Redacted,
    /// For the driver, on the apply thread: the real value (PostgreSQL: its SCRAM-SHA-256 verifier).
    Exec,
}

/// A typed password in a cell shared by every clone.
#[derive(Clone)]
pub struct Password(Arc<Mutex<Option<Secret>>>);

impl Password {
    pub fn new(text: impl Into<String>) -> Self {
        Self(Arc::new(Mutex::new(Some(Secret::new(text.into())))))
    }

    fn cell(&self) -> MutexGuard<'_, Option<Secret>> {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Runs `use_it` with the plain text; `None` once the password has been wiped.
    pub fn with<R>(&self, use_it: impl FnOnce(&str) -> R) -> Option<R> {
        self.cell().as_ref().map(|secret| use_it(secret.expose()))
    }

    /// The password is still there.
    pub fn is_present(&self) -> bool {
        self.cell().is_some()
    }

    /// Zeroizes the password in every clone.
    pub fn wipe(&self) {
        self.cell().take();
    }
}

impl std::fmt::Debug for Password {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.is_present() { "Password(<redacted>)" } else { "Password(<wiped>)" })
    }
}

impl PartialEq for Password {
    fn eq(&self, other: &Self) -> bool {
        if Arc::ptr_eq(&self.0, &other.0) {
            return true;
        }
        let (a, b) = (self.cell(), other.cell());
        match (a.as_ref(), b.as_ref()) {
            (Some(a), Some(b)) => a.expose() == b.expose(),
            (None, None) => true,
            _ => false,
        }
    }
}

impl Eq for Password {}

/// What a change does to the password of an account.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum PasswordChange {
    /// Leaves it as it is.
    #[default]
    Keep,
    /// Sets this one.
    Set(Password),
    /// Removes it (PostgreSQL only).
    Clear,
}

impl PasswordChange {
    /// The password to set, if the change sets one.
    pub fn password(&self) -> Option<&Password> {
        match self {
            Self::Set(password) => Some(password),
            _ => None,
        }
    }

    /// Wipes the password this change holds.
    pub fn wipe(&self) {
        if let Some(password) = self.password() {
            password.wipe();
        }
    }
}

impl Drop for Scrubber {
    /// The needles are copies of the password: they are wiped with the scrubber.
    fn drop(&mut self) {
        self.needles.iter_mut().for_each(wipe_string);
    }
}

/// Removes the text of secrets from a message that may quote the statement that carried them (a syntax error
/// of the server repeats the text around the mistake).
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Scrubber {
    needles: Vec<String>,
}

impl std::fmt::Debug for Scrubber {
    /// The needles are copies of the passwords: only how many there are shows.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Scrubber({} secrets, <redacted>)", self.needles.len())
    }
}

impl Scrubber {
    /// Also hides `secret` (ignored when empty).
    pub fn hide(&mut self, secret: &str) {
        if !secret.is_empty() && !self.needles.iter().any(|needle| needle == secret) {
            self.needles.push(secret.to_owned());
        }
    }

    /// `text` with every secret (whole) replaced by [`REDACTED`]. The longest secrets go first, so a secret that contains another is hidden
    /// whole. Only whole secrets: a start of one is text like any other (a message that names the account `alice` must stay readable
    /// when the password is `alice2024`), and a server's cut-off echo of a statement is handled where the statement is known to carry a
    /// secret ([`Self::without_echo`]).
    pub fn scrub(&self, text: &str) -> String {
        let mut ordered: Vec<&String> = self.needles.iter().collect();
        ordered.sort_by_key(|needle| std::cmp::Reverse(needle.len()));
        ordered.into_iter().fold(text.to_owned(), |text, needle| text.replace(needle.as_str(), REDACTED))
    }

    /// `text`, a server's complaint about a statement that carried a secret, without what it quotes of the statement
    /// (`... near '<the next 80 bytes of the statement>' at line 1 (error 1064, SQLSTATE 42000)`): the words before the quote and the code after
    /// it stay, the quoted text goes. Anything else the message says is left to [`Self::scrub`].
    pub fn without_echo(text: &str) -> String {
        let marker = ["near '", "near \"", "near `"].iter().filter_map(|marker| text.find(marker)).min();
        let Some(at) = marker else { return text.to_owned() };
        let head = text[..at].trim_end();
        // The code the driver appends after the echo, when it has one.
        let tail = text.rfind("(error ").filter(|&pos| pos > at).map(|pos| &text[pos..]).filter(|tail| is_error_code(tail));
        match tail {
            Some(tail) => format!("{head} near <the statement is not shown> {tail}"),
            None => format!("{head} near <the statement is not shown>"),
        }
    }

    /// Adds what a statement carries that its redacted text does not: the literal that stands where the redacted
    /// text has [`REDACTED_LITERAL`] (with and without its quotes). The two texts are the same statement apart from
    /// that literal.
    pub fn hide_difference(&mut self, redacted: &str, exec: &str) {
        let Some(at) = redacted.find(REDACTED_LITERAL) else { return };
        let after = &redacted[at + REDACTED_LITERAL.len()..];
        let Some(end) = exec.len().checked_sub(after.len()).filter(|&end| end >= at && exec.is_char_boundary(end) && exec.ends_with(after)) else {
            return;
        };
        let literal = &exec[at..end];
        self.hide(literal);
        if let Some(inner) = literal.strip_prefix('\'').and_then(|rest| rest.strip_suffix('\'')) {
            self.hide(inner);
        }
    }
}

/// Zeroizes the text of `text` (the engine's [`Secret`] wipes its buffer when it drops).
pub fn wipe_string(text: &mut String) {
    drop(Secret::new(std::mem::take(text)));
}

/// `(error 1064, SQLSTATE 42000)`: the code a driver appends to a MySQL message, and nothing else.
fn is_error_code(tail: &str) -> bool {
    let Some(inner) = tail.trim_end().strip_prefix("(error ").and_then(|rest| rest.strip_suffix(')')) else { return false };
    inner.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, ',' | ' ')) && inner.contains("SQLSTATE")
}
