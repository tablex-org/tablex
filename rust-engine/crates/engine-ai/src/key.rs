//! The API key.

use zeroize::Zeroizing;

/// An API key. Wiped when the last copy drops; `Debug` prints `***`; there is no `Display`. The
/// text is read only inside this crate, when the request header is built.
#[derive(Clone)]
pub struct ApiKey(Zeroizing<String>);

impl ApiKey {
    /// Surrounding whitespace (a pasted newline) is not part of the key: it is trimmed here, so
    /// the text that is sent and the text that is scrubbed from errors are the same. A key of
    /// only whitespace is empty.
    pub fn new(key: impl Into<String>) -> Self {
        // Wrapped first, so the copy that is trimmed away is wiped too.
        let mut key = Zeroizing::new(key.into());
        let end = key.trim_end().len();
        key.truncate(end);
        let leading = key.len() - key.trim_start().len();
        key.drain(..leading);
        Self(key)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The text, for building the header and for scrubbing errors. Crate-private on purpose.
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ApiKey(***)")
    }
}
