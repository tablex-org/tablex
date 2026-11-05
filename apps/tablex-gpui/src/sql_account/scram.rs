//! The SCRAM-SHA-256 verifier PostgreSQL stores for a role (`psql`'s `\password` makes the same text): the password is hashed here and
//! only the verifier goes into `ALTER ROLE ... PASSWORD '...'`, so the plain password never reaches the server, its logs or the
//! statement text.
//!
//! `SCRAM-SHA-256$<iterations>:<salt>$<StoredKey>:<ServerKey>` (RFC 5802 / 7677), from PBKDF2-HMAC-SHA-256 of the password as the server
//! prepares it ([`super::saslprep`]). The server stores a password given in that shape as it is, whatever `password_encryption` says.
//!
//! What is wiped: the prepared copy of the password this code makes (when it differs from the caller's), and the 32-byte PBKDF2 output
//! and the client key (overwritten with volatile writes). What is not: the copies inside the PBKDF2 / HMAC / SHA-2 states of the crates,
//! which have no wipe hook here.

use std::borrow::Cow;

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use pbkdf2::hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

/// PostgreSQL's `scram_iterations` default.
pub const ITERATIONS: u32 = 4096;
pub const SALT_LEN: usize = 16;

fn hmac(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC takes a key of any length");
    mac.update(data);
    mac.finalize().into_bytes().into()
}

/// Overwrites `bytes` with zeros in a way the optimizer may not drop.
fn wipe_bytes(bytes: &mut [u8]) {
    for byte in bytes {
        // SAFETY: `byte` is a valid, aligned, exclusive reference to a `u8`.
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
}

/// The verifier of `prepared` (already as the server prepares it) for `salt`.
fn hash(prepared: &str, salt: &[u8], iterations: u32) -> String {
    let mut salted = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha256>(prepared.as_bytes(), salt, iterations, &mut salted);
    let mut client_key = hmac(&salted, b"Client Key");
    let stored_key = Sha256::digest(client_key);
    let server_key = hmac(&salted, b"Server Key");
    wipe_bytes(&mut salted);
    wipe_bytes(&mut client_key);
    format!("SCRAM-SHA-256${iterations}:{}${}:{}", STANDARD.encode(salt), STANDARD.encode(stored_key), STANDARD.encode(server_key))
}

/// The verifier of `password` for `salt`.
pub fn verifier(password: &str, salt: &[u8], iterations: u32) -> String {
    let prepared = super::saslprep::prepare(password);
    let verifier = hash(&prepared, salt, iterations);
    if let Cow::Owned(mut owned) = prepared {
        super::password::wipe_string(&mut owned);
    }
    verifier
}

/// The verifier of exactly `text`, without any preparation (what a test compares the real one with).
#[cfg(test)]
pub fn verifier_of_exactly(text: &str, salt: &[u8], iterations: u32) -> String {
    hash(text, salt, iterations)
}

/// A fresh random salt (the operating system's generator, through the CSPRNG `uuid` draws a v4 id from).
pub fn random_salt() -> [u8; SALT_LEN] {
    *uuid::Uuid::new_v4().as_bytes()
}

/// The verifier of `password` with a random salt.
pub fn new_verifier(password: &str) -> String {
    verifier(password, &random_salt(), ITERATIONS)
}
