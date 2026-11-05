//! Secret text the front end hands to the tool layer so it can be scrubbed from errors and audit
//! lines. The value is wiped when the last copy drops (the session closing), has no `Display`, and
//! prints nothing in `Debug`: a secret cannot reach a log through this type by accident.

use zeroize::Zeroizing;

/// An exact secret value (password, token, key passphrase).
#[derive(Clone, PartialEq, Eq)]
pub struct SecretText(Zeroizing<String>);

impl SecretText {
    pub fn new(text: impl Into<String>) -> Self {
        Self(Zeroizing::new(text.into()))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Only the redactor reads the text.
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl From<&str> for SecretText {
    fn from(text: &str) -> Self {
        Self::new(text)
    }
}

impl std::fmt::Debug for SecretText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretText(<hidden>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secret_prints_nothing_and_compares_by_value() {
        let secret = SecretText::new("hunter2");
        assert!(!format!("{secret:?} {:?}", vec![secret.clone()]).contains("hunter2"));
        assert_eq!(secret, SecretText::from("hunter2"));
        assert!(SecretText::new("").is_empty());
    }
}
