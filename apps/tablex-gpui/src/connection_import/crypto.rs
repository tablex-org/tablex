//! The ciphers the other tools use for stored passwords and exports. Decrypt
//! only. The keys of Navicat and DBeaver are fixed constants shipped in those
//! products (and in DBX's importers), not secrets.

use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use aes_gcm::{aead::Aead, Aes256Gcm, KeyInit, Nonce};

type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

/// Navicat 12+ password key and IV.
const NAVICAT_KEY: &[u8; 16] = b"libcckeylibcckey";
const NAVICAT_IV: &[u8; 16] = b"libcciv libcciv ";

/// DBeaver's default `credentials-config.json` key (the file starts with the IV).
const DBEAVER_KEY: [u8; 16] = [
    186, 187, 74, 159, 119, 74, 184, 83, 201, 108, 45, 101, 61, 254, 84, 74,
];

fn aes128_cbc_decrypt(key: &[u8; 16], iv: &[u8; 16], data: &[u8]) -> Option<Vec<u8>> {
    let mut buffer = data.to_vec();
    let plain = Aes128CbcDec::new(key.into(), iv.into())
        .decrypt_padded_mut::<Pkcs7>(&mut buffer)
        .ok()?;
    Some(plain.to_vec())
}

/// A Navicat `Password="<hex>"` value. `None` for empty or undecryptable
/// input (older Navicat versions use another cipher).
pub(super) fn navicat_password(hex_value: &str) -> Option<String> {
    let encrypted = hex::decode(hex_value.trim()).ok().filter(|b| !b.is_empty())?;
    String::from_utf8(aes128_cbc_decrypt(NAVICAT_KEY, NAVICAT_IV, &encrypted)?).ok()
}

/// DBeaver's `credentials-config.json`: 16-byte IV, then AES-128-CBC of JSON.
pub(super) fn dbeaver_credentials(file: &[u8]) -> Option<Vec<u8>> {
    let (iv, encrypted) = file.split_at_checked(16)?;
    aes128_cbc_decrypt(&DBEAVER_KEY, iv.try_into().ok()?, encrypted)
}

/// Key of a DBX config export: PBKDF2-HMAC-SHA256 (100 000 rounds) of the
/// passphrase, used as an AES-256-GCM key.
pub(super) fn dbx_key(passphrase: &str, salt: &[u8]) -> [u8; 32] {
    let mut key = [0_u8; 32];
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(passphrase.as_bytes(), salt, 100_000, &mut key);
    key
}

/// AES-256-GCM decryption (96-bit IV, no associated data, 16-byte tag at the
/// end of `data`) as WebCrypto produces it. `None` when the tag does not
/// verify, i.e. a wrong key or a damaged file.
pub(super) fn aes256_gcm_decrypt(key: &[u8; 32], iv: &[u8; 12], data: &[u8]) -> Option<Vec<u8>> {
    Aes256Gcm::new(key.into())
        .decrypt(Nonce::from_slice(iv), data)
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navicat_password_round_trip_with_the_published_key() {
        // "navicat-secret" encrypted with `openssl enc -aes-128-cbc` and the key "libcckeylibcckey", iv "libcciv libcciv ".
        assert_eq!(
            navicat_password("93C3116A8A7050BB3A7981C3075FC1B7").as_deref(),
            Some("navicat-secret")
        );
        assert_eq!(navicat_password(""), None);
        assert_eq!(navicat_password("not hex"), None);
        // Valid hex, wrong padding.
        assert_eq!(navicat_password("00112233445566778899AABBCCDDEEFF"), None);
    }

    #[test]
    fn gcm_matches_nist_test_case_15_and_rejects_tampering() {
        let key: [u8; 32] =
            hex::decode("feffe9928665731c6d6a8f9467308308feffe9928665731c6d6a8f9467308308")
                .unwrap()
                .try_into()
                .unwrap();
        let iv: [u8; 12] = hex::decode("cafebabefacedbaddecaf888")
            .unwrap()
            .try_into()
            .unwrap();
        let plain = hex::decode(
            "d9313225f88406e5a55909c5aff5269a86a7a9531534f7da2e4c303d8a318a72\
             1c3c0c95956809532fcf0e2449a6b525b16aedf5aa0de657ba637b391aafd255",
        )
        .unwrap();
        let sealed = hex::decode(
            "522dc1f099567d07f47f37a32a84427d643a8cdcbfe5c0c97598a2bd2555d1aa\
             8cb08e48590dbb3da7b08b1056828838c5f61e6393ba7a0abcc9f662898015ad\
             b094dac5d93471bdec1a502270e3cc6c",
        )
        .unwrap();
        assert_eq!(aes256_gcm_decrypt(&key, &iv, &sealed), Some(plain));
        let mut tampered = sealed.clone();
        tampered[3] ^= 1;
        assert_eq!(aes256_gcm_decrypt(&key, &iv, &tampered), None);
        assert_eq!(aes256_gcm_decrypt(&key, &iv, &sealed[..10]), None);
    }
}
