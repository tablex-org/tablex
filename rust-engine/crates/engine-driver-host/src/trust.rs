use crate::{DriverManifest, InstalledDriverStore, StoreError, TrustPolicy};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use ed25519_dalek::{Signature, VerifyingKey};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ManifestTrustError {
    #[error("manifest signature is missing")]
    SignatureMissing,
    #[error("manifest signature is malformed: {0}")]
    SignatureMalformed(String),
    #[error("manifest signature does not match any trusted key")]
    SignatureInvalid,
    #[error("no trusted manifest keys are configured")]
    NoTrustedKeys,
    #[error("trusted manifest key is invalid: {0}")]
    InvalidKey(String),
    #[error("signed manifest has no valid generatedAt timestamp")]
    MissingGeneratedAt,
    #[error("manifest generatedAt {generated_at} is older than the last accepted manifest")]
    Replayed { generated_at: String },
    #[error("manifest JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Detached ed25519 signature file location for a manifest source.
pub fn signature_source(manifest_source: &str) -> String {
    format!("{manifest_source}.sig")
}

/// Decode a base64 32-byte ed25519 public key (the form compiled into the app).
pub fn public_key_from_base64(encoded: &str) -> Result<[u8; 32], ManifestTrustError> {
    let bytes = STANDARD
        .decode(encoded.trim())
        .map_err(|error| ManifestTrustError::InvalidKey(error.to_string()))?;
    <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| {
        ManifestTrustError::InvalidKey(format!("expected 32 bytes, got {}", bytes.len()))
    })
}

/// Decode a comma-separated list of base64 ed25519 public keys (several keys
/// allow rotation). Surrounding whitespace is ignored. A blank list is "no
/// keys"; an empty or malformed entry inside a non-blank list is rejected so
/// a typo can never silently drop a key.
pub fn public_keys_from_list(list: &str) -> Result<Vec<[u8; 32]>, ManifestTrustError> {
    if list.trim().is_empty() {
        return Ok(Vec::new());
    }
    list.split(',')
        .enumerate()
        .map(|(index, entry)| {
            public_key_from_base64(entry).map_err(|error| match error {
                ManifestTrustError::InvalidKey(reason) => {
                    ManifestTrustError::InvalidKey(format!("entry {}: {reason}", index + 1))
                }
                other => other,
            })
        })
        .collect()
}

/// Verifies signed manifests against pinned public keys and rejects replays.
///
/// Strict: a valid detached `<manifest>.sig` (base64 ed25519 over the exact
/// manifest bytes) from one of the keys is required and `generatedAt` must not
/// be older than the last accepted manifest (persisted in the store directory).
/// Developer: a `file://` manifest is accepted unsigned (no replay tracking);
/// anything else is verified exactly like Strict.
#[derive(Debug, Clone)]
pub struct ManifestTrust {
    keys: Vec<VerifyingKey>,
    policy: TrustPolicy,
}

impl ManifestTrust {
    pub fn new(public_keys: &[[u8; 32]], policy: TrustPolicy) -> Result<Self, ManifestTrustError> {
        let keys = public_keys
            .iter()
            .map(|bytes| {
                VerifyingKey::from_bytes(bytes)
                    .map_err(|error| ManifestTrustError::InvalidKey(error.to_string()))
            })
            .collect::<Result<_, _>>()?;
        Ok(Self { keys, policy })
    }

    pub fn policy(&self) -> TrustPolicy {
        self.policy
    }

    /// Whether at least one key is pinned (without one, no signature verifies).
    pub fn has_keys(&self) -> bool {
        !self.keys.is_empty()
    }

    /// Whether the registry must fetch and check a `.sig` for this source.
    pub fn requires_signature(&self, manifest_source: &str) -> bool {
        !(self.policy == TrustPolicy::Developer && is_file_source(manifest_source))
    }

    /// Check `manifest` (raw bytes as fetched) and return it parsed.
    pub fn verify(
        &self,
        store: &InstalledDriverStore,
        manifest_source: &str,
        manifest: &[u8],
        signature: Option<&[u8]>,
    ) -> Result<DriverManifest, ManifestTrustError> {
        if !self.requires_signature(manifest_source) {
            return Ok(serde_json::from_slice(manifest)?);
        }
        self.verify_signature(manifest, signature)?;
        let parsed: DriverManifest = serde_json::from_slice(manifest)?;
        let generated_at = parsed
            .generated_at
            .as_deref()
            .ok_or(ManifestTrustError::MissingGeneratedAt)?;
        let seconds =
            parse_unix_seconds(generated_at).ok_or(ManifestTrustError::MissingGeneratedAt)?;
        if !store.accept_manifest_time(seconds)? {
            return Err(ManifestTrustError::Replayed {
                generated_at: generated_at.to_string(),
            });
        }
        Ok(parsed)
    }

    fn verify_signature(
        &self,
        manifest: &[u8],
        signature: Option<&[u8]>,
    ) -> Result<(), ManifestTrustError> {
        if self.keys.is_empty() {
            return Err(ManifestTrustError::NoTrustedKeys);
        }
        let encoded = signature.ok_or(ManifestTrustError::SignatureMissing)?;
        let encoded = std::str::from_utf8(encoded)
            .map_err(|error| ManifestTrustError::SignatureMalformed(error.to_string()))?;
        let bytes = STANDARD
            .decode(encoded.trim())
            .map_err(|error| ManifestTrustError::SignatureMalformed(error.to_string()))?;
        let signature = Signature::from_slice(&bytes)
            .map_err(|error| ManifestTrustError::SignatureMalformed(error.to_string()))?;
        if self
            .keys
            .iter()
            .any(|key| key.verify_strict(manifest, &signature).is_ok())
        {
            Ok(())
        } else {
            Err(ManifestTrustError::SignatureInvalid)
        }
    }
}

fn is_file_source(source: &str) -> bool {
    source
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("file://"))
}

/// Seconds since the Unix epoch for `YYYY-MM-DDTHH:MM:SS[.fraction](Z|±HH:MM)`.
pub(crate) fn parse_unix_seconds(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    let digits = |range: std::ops::Range<usize>| -> Option<i64> {
        let slice = bytes.get(range)?;
        slice.iter().all(u8::is_ascii_digit).then(|| {
            slice
                .iter()
                .fold(0, |acc, d| acc * 10 + i64::from(d - b'0'))
        })
    };
    let separators_ok = bytes.get(4) == Some(&b'-')
        && bytes.get(7) == Some(&b'-')
        && bytes.get(10) == Some(&b'T')
        && bytes.get(13) == Some(&b':')
        && bytes.get(16) == Some(&b':');
    if !separators_ok {
        return None;
    }
    let (year, month, day) = (digits(0..4)?, digits(5..7)?, digits(8..10)?);
    let (hour, minute, second) = (digits(11..13)?, digits(14..16)?, digits(17..19)?);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let mut rest = &text[19..];
    if let Some(fraction) = rest.strip_prefix('.') {
        let end = fraction
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(fraction.len());
        if end == 0 {
            return None;
        }
        rest = &fraction[end..];
    }
    let offset = match rest.as_bytes() {
        b"Z" => 0,
        [sign @ (b'+' | b'-'), h1, h2, b':', m1, m2]
            if [h1, h2, m1, m2].iter().all(|d| d.is_ascii_digit()) =>
        {
            let pair = |tens: &u8, ones: &u8| i64::from(tens - b'0') * 10 + i64::from(ones - b'0');
            let magnitude = pair(h1, h2) * 3600 + pair(m1, m2) * 60;
            if *sign == b'+' {
                magnitude
            } else {
                -magnitude
            }
        }
        _ => return None,
    };
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second - offset)
}

/// Howard Hinnant's days_from_civil.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::{parse_unix_seconds, public_keys_from_list, ManifestTrustError};
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;

    fn key(byte: u8) -> String {
        STANDARD.encode([byte; 32])
    }

    #[test]
    fn a_key_list_holds_several_keys_in_order_and_ignores_whitespace() {
        let list = format!(" {} ,\n{}\t,{}", key(1), key(2), key(3));
        assert_eq!(
            public_keys_from_list(&list).expect("three keys"),
            vec![[1; 32], [2; 32], [3; 32]]
        );
        assert_eq!(public_keys_from_list(&key(9)).expect("one key"), vec![[9; 32]]);
    }

    #[test]
    fn a_blank_key_list_means_no_keys() {
        for list in ["", "   ", "\n"] {
            assert!(public_keys_from_list(list).expect("blank").is_empty(), "{list:?}");
        }
    }

    #[test]
    fn an_invalid_entry_rejects_the_whole_list_and_names_it() {
        let truncated = STANDARD.encode([7_u8; 31]);
        for (list, entry) in [
            (format!("{},not-base64!", key(1)), 2),
            (format!("{truncated},{}", key(1)), 1),
            (format!("{},,{}", key(1), key(2)), 2),
            (format!("{},", key(1)), 2),
        ] {
            match public_keys_from_list(&list) {
                Err(ManifestTrustError::InvalidKey(reason)) => {
                    assert!(reason.starts_with(&format!("entry {entry}:")), "{list}: {reason}")
                }
                other => panic!("{list}: expected InvalidKey, got {other:?}"),
            }
        }
    }

    #[test]
    fn parses_rfc3339_with_zones_and_fractions() {
        assert_eq!(parse_unix_seconds("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_unix_seconds("2026-10-08T12:00:00Z"),
            Some(1_791_460_800)
        );
        assert_eq!(
            parse_unix_seconds("2026-10-08T14:00:00.250+02:00"),
            Some(1_791_460_800)
        );
        assert_eq!(parse_unix_seconds("2026-10-08T12:00:00"), None);
        assert_eq!(parse_unix_seconds("yesterday"), None);
        assert_eq!(parse_unix_seconds("2026-13-08T12:00:00Z"), None);
    }
}
