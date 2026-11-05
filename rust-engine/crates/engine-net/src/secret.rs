//! A string that never prints itself and wipes its memory when dropped.

use std::fmt;

use zeroize::Zeroize;

/// Password, key passphrase or keyboard-interactive answer.
///
/// `Debug` is redacted, there is no `Display` and no serde impl, so a secret cannot reach a log line or a
/// persisted file by accident. `Secret` zeroizes **its own** buffer on drop.
///
/// Known limit: the password and keyboard-interactive answers must be handed to russh as plain `String`s.
/// russh 0.64 zeroizes its copy of the `password`-method string but **not** the copy it makes for a
/// keyboard-interactive `AuthInfoResponse`, so such an answer can remain in freed heap memory until it is
/// overwritten. russh's API offers no way to avoid or clear that copy, so there is no mitigation here.
#[derive(Clone, Default)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The raw value. Call only at the point of use (handing it to the SSH library).
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<String> for Secret {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for Secret {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_is_redacted() {
        let secret = Secret::new("hunter2-needle");
        assert!(!format!("{secret:?}").contains("hunter2"));
        assert!(!format!("{secret:#?}").contains("hunter2"));
        assert_eq!(secret.expose(), "hunter2-needle");
    }

    #[test]
    fn zeroize_wipes_the_buffer() {
        let mut value = String::from("abcdef");
        value.zeroize();
        assert!(value.is_empty());
    }
}
