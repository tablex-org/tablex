//! Scrubbing secrets from text that leaves the tool layer (error messages,
//! audit lines).
//!
//! Three things are masked, in this order:
//!
//! 1. exact secret values the front end registered (`Session::secrets`), when they are at least
//!    [`MIN_EXACT_SECRET_CHARS`] long (see there for why shorter ones are left to the shape rules);
//!    a secret made of identifier characters only is masked where it is a word of its own, not
//!    inside a longer one, unless it has [`LONG_SECRET_CHARS`] or more characters (see there);
//! 2. the user-info part of any `scheme://user:password@host` URI;
//! 3. a value that follows a secret-looking key: `password=...`,
//!    `token: ...`, `PASSWORD '...'`, `IDENTIFIED BY '...'`.
//!
//! It is a safety net for text the driver or the user wrote, not a parser:
//! the connection URIs and keys themselves never reach the tool layer.

use crate::secret::SecretText;
use std::borrow::Cow;

pub const MASK: &str = "***";

/// Exact-value masking (and the percent-encoded forms) only applies to secrets of at least this
/// many characters. **Trade-off:** masking a 1-character password everywhere would corrupt every
/// DDL, error and audit text that happens to contain the character, and a password equal to a table
/// or user name would show up as `***` exactly where `name` shows the word, disclosing it by
/// comparison. A shorter secret is still masked by shape (URI user-info, `password=` and the
/// other key rules below), which is where a driver echoes one.
pub const MIN_EXACT_SECRET_CHARS: usize = 8;

/// A secret of identifier characters only (`[A-Za-z0-9_]`) and fewer than this many characters is
/// masked only where it is a word of its own (the characters around it are not identifier
/// characters): a stock password such as `rootroot` or `password` would otherwise corrupt
/// `rootroot_backup` or `password_hash` in DDL. From this length on it is masked wherever it
/// occurs: a long random value inside a longer run (`tok_xS3cretLongValue1y_end`) is far more
/// likely a leak than a coincidence. A secret with any other character, and the percent-encoded
/// forms, are masked wherever they occur. (The layer also never masks a secret equal to the
/// connection's user or database name, see `SessionProvider::plain_words`.)
pub const LONG_SECRET_CHARS: usize = 16;

/// Keys whose value is masked when it follows with `=`, `:` or a quote.
const SECRET_KEYS: &[SecretKey] = &[
    SecretKey::prefixed("password"),
    SecretKey::prefixed("passwd"),
    SecretKey::prefixed("pwd"),
    SecretKey::bare("secret"),
    SecretKey::bare("token"),
    SecretKey::prefixed("api_key"),
    SecretKey::prefixed("apikey"),
    SecretKey::prefixed("api-key"),
    SecretKey::prefixed("access_key"),
    SecretKey::bare("passphrase"),
    SecretKey::bare("bearer"),
];

/// One entry of [`SECRET_KEYS`].
struct SecretKey {
    word: &'static str,
    /// A key that may carry a word prefix and spaces around its separator (`sslpassword = x`,
    /// `db_password=x`). The other (bare) words are common column and flag names: at the start of
    /// a word they are keys as ever, but behind a word prefix (`client_secret=abc`,
    /// `csrf_token: abc`) only when the separator follows at once, so `CHECK (is_secret = false)`
    /// is not touched.
    prefixed: bool,
}

impl SecretKey {
    const fn prefixed(word: &'static str) -> Self {
        Self {
            word,
            prefixed: true,
        }
    }

    const fn bare(word: &'static str) -> Self {
        Self {
            word,
            prefixed: false,
        }
    }
}

/// How the value after a key is read.
#[derive(Clone, Copy, PartialEq, Eq)]
enum KeyKind {
    /// `key=value`, `key: value`, `KEY 'value'`.
    Plain,
    /// A bare word behind a word prefix: only `key=value` / `key: value` with the separator right
    /// after the key.
    Tight,
    /// `Authorization: Bearer abc def`: the rest of the line (or quoted value).
    Authorization,
}

/// `secret` percent-encoded the way a URI userinfo or query carries it
/// (upper- and lower-case hex), when that differs from the secret itself.
fn percent_forms(secret: &str) -> Vec<String> {
    let encode = |upper: bool| {
        let mut out = String::with_capacity(secret.len() * 3);
        for byte in secret.bytes() {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
                out.push(byte as char);
            } else if upper {
                let _ = std::fmt::Write::write_fmt(&mut out, format_args!("%{byte:02X}"));
            } else {
                let _ = std::fmt::Write::write_fmt(&mut out, format_args!("%{byte:02x}"));
            }
        }
        out
    };
    let mut forms = vec![encode(true), encode(false)];
    forms.retain(|form| form != secret);
    forms.dedup();
    forms
}

/// [`Masks::new`] and [`Masks::redact`] in one: prepares the forms on every call. Use [`Masks`]
/// where there is more than one text to scrub for the same secrets.
pub fn redact(text: &str, secrets: &[SecretText]) -> String {
    Masks::new(secrets).redact(text)
}

/// The values to mask for one set of secrets, prepared once: every form of every secret, longest
/// first across all secrets (a secret that is a prefix of another, `Summer2024!` and
/// `Summer2024!-ssh`, must not be masked before the longer one, or the rest of the longer one,
/// `-ssh`, is left behind). Each form carries a flag: mask it only where it is a word of its own
/// (see [`is_word_like`]). The forms are secrets too and are wiped like them.
#[derive(Clone, Default)]
pub struct Masks {
    forms: Vec<(SecretText, bool)>,
}

impl Masks {
    pub fn new(secrets: &[SecretText]) -> Self {
        let mut forms: Vec<(SecretText, bool)> = Vec::new();
        for secret in secrets
            .iter()
            .filter(|secret| secret.expose().chars().count() >= MIN_EXACT_SECRET_CHARS)
        {
            let exposed = secret.expose();
            if is_word_like(exposed) {
                forms.push((secret.clone(), true));
            } else {
                forms.push((secret.clone(), false));
                forms.extend(
                    percent_forms(exposed)
                        .into_iter()
                        .map(|form| (SecretText::new(form), false)),
                );
            }
        }
        forms.sort_by_key(|(form, _)| std::cmp::Reverse(form.expose().len()));
        Self { forms }
    }

    /// Scrubs `text` of the secrets, then of URI credentials and values behind secret-looking keys.
    pub fn redact(&self, text: &str) -> String {
        let mut out: Cow<'_, str> = Cow::Borrowed(text);
        for (form, whole_word) in &self.forms {
            let form = form.expose();
            if *whole_word {
                out = mask_whole_words(out, form);
            } else if out.contains(form) {
                out = Cow::Owned(out.replace(form, MASK));
            }
        }
        let out = mask_uri_credentials(&out);
        mask_key_values(&out)
    }
}

fn is_identifier_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// A secret that can be a word of an ordinary text (identifier characters only, shorter than
/// [`LONG_SECRET_CHARS`]).
fn is_word_like(secret: &str) -> bool {
    secret.chars().count() < LONG_SECRET_CHARS && secret.chars().all(is_identifier_char)
}

/// Masks `secret` where it is not part of a longer word.
fn mask_whole_words<'a>(text: Cow<'a, str>, secret: &str) -> Cow<'a, str> {
    let mut out = String::new();
    let mut copied = 0;
    for (start, _) in text.match_indices(secret) {
        let end = start + secret.len();
        let inside_word = text[..start]
            .chars()
            .next_back()
            .is_some_and(is_identifier_char)
            || text[end..].chars().next().is_some_and(is_identifier_char);
        if inside_word {
            continue;
        }
        out.push_str(&text[copied..start]);
        out.push_str(MASK);
        copied = end;
    }
    if copied == 0 {
        return text;
    }
    out.push_str(&text[copied..]);
    Cow::Owned(out)
}

/// `scheme://user:pass@host` -> `scheme://***@host`.
fn mask_uri_credentials(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("://") {
        let (head, tail) = rest.split_at(at + 3);
        out.push_str(head);
        let end = tail
            .find(|c: char| {
                // An apostrophe is part of a password (`it's`), not the end of the authority.
                c.is_whitespace() || matches!(c, '/' | '?' | '#' | '"' | '<' | '>')
            })
            .unwrap_or(tail.len());
        let authority = &tail[..end];
        match authority.rfind('@') {
            Some(at_sign) => {
                out.push_str(MASK);
                out.push_str(&authority[at_sign..]);
            }
            None => out.push_str(authority),
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

fn is_word_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
}

/// Masks the value after each secret-looking key.
fn mask_key_values(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < text.len() {
        let key = secret_key_at(&lower, bytes, index);
        let Some((key_len, kind)) = key else {
            let ch = text[index..].chars().next().unwrap_or(' ');
            out.push(ch);
            index += ch.len_utf8();
            continue;
        };
        let key_end = index + key_len;
        out.push_str(&text[index..key_end]);
        index = key_end;
        // Optional closing quote of a quoted key, whitespace, then `=` / `:`
        // (or, for an unquoted key, directly a quoted literal).
        let mut probe = index;
        let key_quoted = probe < bytes.len() && matches!(bytes[probe], b'"' | b'\'');
        if key_quoted {
            probe += 1;
        }
        let spaced = bytes.get(probe) == Some(&b' ');
        while probe < bytes.len() && bytes[probe] == b' ' {
            probe += 1;
        }
        let separator = probe < bytes.len() && matches!(bytes[probe], b'=' | b':');
        let literal = !key_quoted && probe < bytes.len() && matches!(bytes[probe], b'\'' | b'"');
        if (!separator && !literal) || (kind == KeyKind::Tight && (spaced || !separator)) {
            continue;
        }
        if separator {
            probe += 1;
            while probe < bytes.len() && bytes[probe] == b' ' {
                probe += 1;
            }
        }
        out.push_str(&text[index..probe]);
        index = probe;
        index = if kind == KeyKind::Authorization && !matches!(bytes.get(index), Some(b'\'' | b'"'))
        {
            mask_line_value(text, index, &mut out)
        } else {
            mask_value(text, index, &mut out)
        };
    }
    out
}

/// Appends the mask for the value starting at `start`; returns where it ends.
fn mask_value(text: &str, start: usize, out: &mut String) -> usize {
    let bytes = text.as_bytes();
    if start >= bytes.len() {
        return start;
    }
    if matches!(bytes[start], b'\'' | b'"') {
        let quote = bytes[start];
        let mut end = start + 1;
        while end < bytes.len() {
            if bytes[end] == quote {
                if bytes.get(end + 1) == Some(&quote) {
                    end += 2;
                    continue;
                }
                end += 1;
                break;
            }
            end += 1;
        }
        out.push(quote as char);
        out.push_str(MASK);
        if end > start + 1 && bytes[end - 1] == quote {
            out.push(quote as char);
        }
        return end;
    }
    // The value ends at a delimiter, or at a quote that closes a literal the key sits in
    // (`COMMENT='reset token: 24h'`): a quote followed by a word character is part of the value
    // (`it's`), so an apostrophe inside a password does not cut it.
    let end = text[start..]
        .char_indices()
        .find(|(offset, c)| {
            c.is_whitespace()
                || matches!(c, '&' | ';' | ',' | ')')
                || (matches!(c, '\'' | '"')
                    && !text[start + offset + 1..]
                        .chars()
                        .next()
                        .is_some_and(is_identifier_char))
        })
        .map_or(text.len(), |(offset, _)| start + offset);
    if end > start {
        out.push_str(MASK);
    }
    end
}

/// Masks an unquoted value that runs to the end of the line, a quote or a comma (`Authorization:
/// Bearer abc def`); returns where it ends.
fn mask_line_value(text: &str, start: usize, out: &mut String) -> usize {
    let end = text[start..]
        .find(['\n', '\r', '"', ','])
        .map_or(text.len(), |len| start + len);
    if end > start {
        out.push_str(MASK);
    }
    end
}

/// Length and kind of a secret key (or `identified by`) starting at `index`. A key may carry a word
/// prefix (`sslpassword=`, `db_password=`, `PGPASSWORD=`; for the bare words see [`SecretKey`]) but
/// must end its word: `password_changed_at` and `passwords` are not keys (so their values are left
/// alone).
fn secret_key_at(lower: &str, bytes: &[u8], index: usize) -> Option<(usize, KeyKind)> {
    let rest = &lower[index..];
    let word_start = index == 0 || !is_word_char(bytes[index - 1]);
    if word_start && rest.starts_with("identified by") {
        return Some(("identified by".len(), KeyKind::Plain));
    }
    let ends_word = |len: usize| {
        !bytes
            .get(index + len)
            .is_some_and(|next| is_word_char(*next))
    };
    if word_start && rest.starts_with("authorization") && ends_word("authorization".len()) {
        return Some(("authorization".len(), KeyKind::Authorization));
    }
    SECRET_KEYS
        .iter()
        .find(|key| rest.starts_with(key.word) && ends_word(key.word.len()))
        .map(|key| {
            let kind = if key.prefixed || word_start {
                KeyKind::Plain
            } else {
                KeyKind::Tight
            };
            (key.word.len(), kind)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean(text: &str) -> String {
        redact(text, &[])
    }

    #[test]
    fn uri_credentials_are_masked() {
        assert_eq!(
            clean("connect to postgres://admin:hunter2@db.internal:5432/app failed"),
            "connect to postgres://***@db.internal:5432/app failed"
        );
        assert_eq!(clean("redis://:s3cret@host:6379"), "redis://***@host:6379");
        assert_eq!(clean("mysql://u@h/db"), "mysql://***@h/db");
        assert_eq!(
            clean("see https://example.com/page"),
            "see https://example.com/page"
        );
        assert_eq!(
            clean("a mongodb://u:p%40ss@h1,h2/db?x=1 and redis://x:y@z"),
            "a mongodb://***@h1,h2/db?x=1 and redis://***@z"
        );
    }

    #[test]
    fn key_values_are_masked() {
        assert_eq!(
            clean("host=db password=hunter2 port=1"),
            "host=db password=*** port=1"
        );
        assert_eq!(
            clean("?sslmode=require&password=abc&x=1"),
            "?sslmode=require&password=***&x=1"
        );
        assert_eq!(clean("token: abc123 next"), "token: *** next");
        assert_eq!(
            clean("ALTER USER bob PASSWORD 'p@ss word'"),
            "ALTER USER bob PASSWORD '***'"
        );
        assert_eq!(
            clean("CREATE USER x IDENTIFIED BY 'pw1' REQUIRE SSL"),
            "CREATE USER x IDENTIFIED BY '***' REQUIRE SSL"
        );
        assert_eq!(clean("{\"password\": \"abc\"}"), "{\"password\": \"***\"}");
    }

    #[test]
    fn ordinary_sql_is_left_alone() {
        for text in [
            "SELECT password FROM users WHERE id = 1",
            "SELECT * FROM tokens",
            "SELECT author FROM books",
            "UPDATE t SET passwords = 1",
            "password authentication failed for user \"bob\"",
        ] {
            assert_eq!(clean(text), text);
        }
    }

    #[test]
    fn percent_encoded_secrets_are_masked_too() {
        let secrets = vec![SecretText::from("p@ss/w rd:1")];
        for echoed in ["p%40ss%2Fw%20rd%3A1", "p%40ss%2fw%20rd%3a1", "p@ss/w rd:1"] {
            let masked = redact(
                &format!("connect failed for user x with {echoed} at h"),
                &secrets,
            );
            assert_eq!(
                masked, "connect failed for user x with *** at h",
                "{echoed}"
            );
        }
    }

    #[test]
    fn exact_secrets_are_masked_everywhere() {
        let secrets = vec![SecretText::from("Zx9!qq-long"), SecretText::from("")];
        assert_eq!(
            redact("bad login Zx9!qq-long / Zx9!qq-long", &secrets),
            "bad login *** / ***"
        );
    }

    #[test]
    fn a_short_secret_is_masked_by_shape_only_never_by_value() {
        for short in ["1", "users", "abc123"] {
            let secrets = vec![SecretText::from(short)];
            // A table or column that happens to read like the secret is left alone ...
            assert_eq!(
                redact("CREATE TABLE users (id integer, n1 text)", &secrets),
                "CREATE TABLE users (id integer, n1 text)",
                "{short}"
            );
            // ... while the places a driver echoes one are masked by shape.
            for text in [
                format!("connect failed: postgres://app:{short}@db.internal/x"),
                format!("host=db password={short} port=1"),
                format!("ALTER USER bob PASSWORD '{short}'"),
            ] {
                assert!(!text.is_empty());
                let masked = redact(&text, &secrets);
                assert!(
                    !masked.contains(&format!(":{short}@"))
                        && !masked.contains(&format!("password={short}"))
                        && !masked.contains(&format!("'{short}'")),
                    "{masked}"
                );
                assert!(masked.contains(MASK), "{masked}");
            }
        }
        // Exactly at the minimum, the value is masked everywhere.
        let eight = vec![SecretText::from("Ab3dEf7h")];
        assert_eq!(MIN_EXACT_SECRET_CHARS, 8);
        assert_eq!(redact("x Ab3dEf7h y", &eight), "x *** y");
        let seven = vec![SecretText::from("Ab3dEf7")];
        assert_eq!(redact("x Ab3dEf7 y", &seven), "x Ab3dEf7 y");
    }

    #[test]
    fn an_apostrophe_in_a_password_does_not_end_the_user_info() {
        assert_eq!(
            clean("failed for postgres://u:it's@h:5432/db"),
            "failed for postgres://***@h:5432/db"
        );
        assert_eq!(
            clean("dsn='postgres://u:it's@h:5432' and more"),
            "dsn='postgres://***@h:5432' and more"
        );
    }

    #[test]
    fn keys_with_a_word_prefix_and_more_secret_words_are_masked() {
        for (text, expected) in [
            ("sslpassword=abc rest", "sslpassword=*** rest"),
            ("db_password=abc rest", "db_password=*** rest"),
            ("PGPASSWORD=abc rest", "PGPASSWORD=*** rest"),
            ("passphrase: abc rest", "passphrase: *** rest"),
            ("client_secret=abc&x=1", "client_secret=***&x=1"),
            ("csrf_token: abc", "csrf_token: ***"),
            (
                "Authorization: Bearer abc.def.ghi\nnext",
                "Authorization: ***\nnext",
            ),
            ("bearer=abc.def", "bearer=***"),
            (
                "\"authorization\": \"Basic Zm9v\"",
                "\"authorization\": \"***\"",
            ),
        ] {
            assert_eq!(clean(text), expected, "{text}");
        }
        // Pinned: a key that continues into another word is not a secret key, so its value stays;
        // an ordinary identifier or a SQL reference to the column is never touched.
        for text in [
            "password_changed_at = 2024",
            "SELECT bearer FROM t",
            "SELECT passphrase FROM keys",
            "tokens = 5",
            "the bearer of this",
        ] {
            assert_eq!(clean(text), text, "{text}");
        }
    }

    // ---- exact secrets are masked as words of their own (B-5) -----------------------------

    #[test]
    fn a_word_like_secret_is_masked_only_where_it_is_a_word_of_its_own() {
        let secrets = vec![SecretText::from("rootroot")];
        // Inside a longer identifier it is a table or a column, not the password.
        for text in [
            "CREATE TABLE rootroot_backup (rootroot2 integer, myrootroot text)",
            "GRANT ALL ON rootroot9 TO app",
        ] {
            assert_eq!(redact(text, &secrets), text);
        }
        // As a word of its own it is masked, whatever surrounds it.
        for (text, expected) in [
            ("rejected rootroot for app", "rejected *** for app"),
            ("(rootroot)", "(***)"),
            ("'rootroot'", "'***'"),
            ("rootroot", "***"),
            ("a rootroot_x rootroot.", "a rootroot_x ***."),
        ] {
            assert_eq!(redact(text, &secrets), expected, "{text}");
        }
    }

    #[test]
    fn a_secret_that_is_a_prefix_of_another_leaves_nothing_of_the_longer_one_in_any_order() {
        let text = "a Summer2024!-ssh b Summer2024! c Summer2024%21-ssh d";
        let pair = [
            SecretText::from("Summer2024!"),
            SecretText::from("Summer2024!-ssh"),
        ];
        let reversed = [pair[1].clone(), pair[0].clone()];
        for secrets in [&pair[..], &reversed[..]] {
            assert_eq!(redact(text, secrets), "a *** b *** c *** d");
        }
        // The same when the shorter one is word-like (masked as a word of its own).
        let word = [
            SecretText::from("Summer2024"),
            SecretText::from("Summer2024!-ssh"),
        ];
        let reversed = [word[1].clone(), word[0].clone()];
        for secrets in [&word[..], &reversed[..]] {
            assert_eq!(
                redact("x Summer2024!-ssh y Summer2024 z", secrets),
                "x *** y *** z"
            );
        }
    }

    #[test]
    fn a_long_secret_is_masked_even_inside_a_longer_run() {
        // Pinned: from LONG_SECRET_CHARS (16) on a secret is masked wherever it occurs, because a
        // long random value inside a token is far more likely a leak than a coincidence.
        assert_eq!(LONG_SECRET_CHARS, 16);
        let long = vec![SecretText::from("xS3cretLongValue1y")];
        assert!("xS3cretLongValue1y".len() >= LONG_SECRET_CHARS);
        assert_eq!(
            redact("tok_xS3cretLongValue1y_end and axS3cretLongValue1yb", &long),
            "tok_***_end and a***b"
        );
        let sixteen = vec![SecretText::from("S3cretLongValue1")];
        assert_eq!(redact("aS3cretLongValue1b", &sixteen), "a***b");
        // One character shorter is word-like: it stays inside a longer word.
        let fifteen = vec![SecretText::from("S3cretLongValue")];
        assert_eq!(redact("aS3cretLongValueb", &fifteen), "aS3cretLongValueb");
        assert_eq!(redact("x S3cretLongValue y", &fifteen), "x *** y");
        // A secret with a character that is not an identifier character never reads as a word
        // of an ordinary text: masked wherever it occurs.
        let special = vec![SecretText::from("Zx9!qq-long")];
        assert_eq!(redact("aZx9!qq-longb", &special), "a***b");
    }

    #[test]
    fn a_name_password_is_still_hidden_by_shape() {
        // The layer does not hand "postgres" over as a secret when it is also the user and the
        // database name (`SessionProvider::plain_words`); the shape rules still hide it where a
        // driver echoes it as a credential.
        assert_eq!(
            clean("postgresql://postgres:postgres@h/postgres"),
            "postgresql://***@h/postgres"
        );
        assert_eq!(clean("password=postgres"), "password=***");
        assert_eq!(
            clean("OWNER TO postgres; database \"postgres\" does not exist"),
            "OWNER TO postgres; database \"postgres\" does not exist"
        );
    }

    // ---- less over-masking in DDL and audit text (B-6) ---------------------------------------

    #[test]
    fn a_column_or_flag_that_merely_ends_in_a_secret_word_is_left_alone() {
        for text in [
            "CHECK (is_secret = false)",
            "CHECK (is_secret = false AND x > 1)",
            "ALTER TABLE t ADD CONSTRAINT c CHECK (has_token = true)",
            "WHERE csrf_token = 'x'",
        ] {
            assert_eq!(clean(text), text, "{text}");
        }
        // A key=value written without spaces after a word prefix is a credential.
        assert_eq!(clean("is_secret=hunter2)"), "is_secret=***)");
    }

    #[test]
    fn a_masked_value_never_eats_the_closing_quote_or_bracket() {
        for (text, expected) in [
            ("COMMENT='reset token: 24h'", "COMMENT='reset token: ***'"),
            (
                "COMMENT \"the token: abc\" ON t",
                "COMMENT \"the token: ***\" ON t",
            ),
            ("f(token: abc)", "f(token: ***)"),
            ("f(a, token=abc, b)", "f(a, token=***, b)"),
            ("{\"msg\": \"password=abc\"}", "{\"msg\": \"password=***\"}"),
        ] {
            assert_eq!(clean(text), expected, "{text}");
        }
        // An apostrophe inside an unquoted value is part of it.
        assert_eq!(clean("password=it's more"), "password=*** more");
    }

    #[test]
    fn a_uri_inside_a_sql_concatenation_is_over_masked_on_purpose() {
        // Pinned: the user-info of a URI ends at its last `@` before whitespace, `/`, `?`, `#`, a
        // double quote or an angle bracket; a single quote does not end it, because a password
        // may contain one (`it's`). So a `'@'` later in the same token reaches back and masks
        // the host. This is the safe side: it hides too much, never too little.
        assert_eq!(
            clean("'jdbc:mysql://h:3306'||u||'@'||x"),
            "'jdbc:mysql://***@'||x"
        );
        assert_eq!(clean("'jdbc:mysql://h:3306'"), "'jdbc:mysql://h:3306'");
    }

    #[test]
    fn prepared_masks_scrub_exactly_like_redact() {
        let secrets = [
            SecretText::from("Summer2024!"),
            SecretText::from("Summer2024!-ssh"),
            SecretText::from("rootroot"),
            SecretText::from("p@ss/w rd:1"),
        ];
        let masks = Masks::new(&secrets);
        for text in [
            "a Summer2024!-ssh b Summer2024! c rootroot d rootroot_backup",
            "uri p%40ss%2Fw%20rd%3A1 and p@ss/w rd:1; password=x",
            "nothing to hide here",
            "",
        ] {
            assert_eq!(masks.redact(text), redact(text, &secrets), "{text}");
        }
    }

    /// `cargo test -p engine-agent --release redact::tests::masking_cost -- --ignored --nocapture`.
    /// A column's comment and default are scrubbed on every `describe_table`, up to hundreds of
    /// tables a `get_schema_context`: before, the forms (a vector, the percent-encoded variants
    /// and a sort) were rebuilt for each text.
    #[test]
    #[ignore = "timing; run by hand"]
    fn masking_cost() {
        let secrets = [
            SecretText::from("Summer2024!-ssh"),
            SecretText::from("p@ss/w rd:1 and more"),
            SecretText::from("rootroot"),
        ];
        let cells: Vec<String> = (0..4000)
            .map(|i| {
                format!("column {i} comment: the number of rows, default nextval('t_{i}_seq')")
            })
            .collect();
        let masks = Masks::new(&secrets);
        let median = |mut run: Box<dyn FnMut() -> usize>| {
            let mut times: Vec<_> = (0..7)
                .map(|_| {
                    let started = std::time::Instant::now();
                    let bytes = run();
                    assert!(bytes > 0);
                    started.elapsed()
                })
                .collect();
            times.sort();
            times[3]
        };
        let rebuilt = {
            let (cells, secrets) = (cells.clone(), secrets.clone());
            median(Box::new(move || {
                cells.iter().map(|cell| redact(cell, &secrets).len()).sum()
            }))
        };
        let prepared = {
            let cells = cells.clone();
            let masks = masks.clone();
            median(Box::new(move || {
                cells.iter().map(|cell| masks.redact(cell).len()).sum()
            }))
        };
        eprintln!(
            "masking cost over {} cells (median of 7): forms rebuilt per text {:?} ({:.0} ns/text), \
             prepared once {:?} ({:.0} ns/text)",
            cells.len(),
            rebuilt,
            rebuilt.as_nanos() as f64 / cells.len() as f64,
            prepared,
            prepared.as_nanos() as f64 / cells.len() as f64,
        );
    }
}
