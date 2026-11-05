//! Dev signing for driver manifests (ed25519, detached base64 `.sig`).
//!
//! ```text
//! cargo run -p engine-driver-host --example sign-manifest -- keygen <private-key-file>
//! cargo run -p engine-driver-host --example sign-manifest -- pubkey <private-key-file>
//! cargo run -p engine-driver-host --example sign-manifest -- sign <private-key-file> <manifest.json> [sig-file]
//! ```
//!
//! * `keygen` writes a new 32-byte seed (base64, mode 0600; refuses to
//!   overwrite) and prints the base64 public key to compile into the app.
//! * `pubkey` prints the public key of an existing key file.
//! * `sign` writes the signature to `<manifest>.sig` unless `sig-file` is
//!   given, then verifies it back. A key file named `-` is read from standard input, so a CI
//!   secret never touches the disk (`driver_manifest.py sign --key-env`). The signature covers the exact manifest
//!   bytes, so sign after the manifest file is final.
//!
//! Helper for `rust-engine/scripts`; not shipped. Production keys belong in CI
//! secrets, not in a file produced here.

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["keygen", key] => keygen(Path::new(key)),
        ["pubkey", key] => load_key(Path::new(key)).map(|key| public_key(&key)),
        ["sign", key, manifest] => sign(Path::new(key), Path::new(manifest), None),
        ["sign", key, manifest, sig] => {
            sign(Path::new(key), Path::new(manifest), Some(Path::new(sig)))
        }
        _ => Err(
            "usage: sign-manifest keygen|pubkey <key-file> | sign <key-file> <manifest> [sig-file]"
                .into(),
        ),
    };
    match result {
        Ok(output) => {
            println!("{output}");
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

/// A new key from `/dev/urandom`, written owner-only (0600): both are Unix, and so is the tool.
#[cfg(not(unix))]
fn keygen(_path: &Path) -> Result<String, String> {
    Err("keygen needs a Unix system (/dev/urandom, an owner-only key file)".into())
}

#[cfg(unix)]
fn keygen(path: &Path) -> Result<String, String> {
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let mut seed = [0_u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut random| random.read_exact(&mut seed))
        .map_err(|error| format!("cannot read randomness: {error}"))?;
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| format!("cannot create {}: {error}", path.display()))?;
    writeln!(file, "{}", STANDARD.encode(seed)).map_err(|error| error.to_string())?;
    Ok(public_key(&SigningKey::from_bytes(&seed)))
}

fn load_key(path: &Path) -> Result<SigningKey, String> {
    let text = if path == Path::new("-") {
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .map_err(|error| format!("cannot read the key from standard input: {error}"))?;
        text
    } else {
        std::fs::read_to_string(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?
    };
    let seed = STANDARD
        .decode(text.trim())
        .map_err(|error| format!("key file is not base64: {error}"))?;
    let seed = <[u8; 32]>::try_from(seed.as_slice())
        .map_err(|_| format!("key must be 32 bytes, got {}", seed.len()))?;
    Ok(SigningKey::from_bytes(&seed))
}

fn public_key(key: &SigningKey) -> String {
    STANDARD.encode(key.verifying_key().to_bytes())
}

fn sign(key: &Path, manifest: &Path, sig: Option<&Path>) -> Result<String, String> {
    let key = load_key(key)?;
    let bytes = std::fs::read(manifest)
        .map_err(|error| format!("cannot read {}: {error}", manifest.display()))?;
    let signature = key.sign(&bytes);
    key.verifying_key()
        .verify_strict(&bytes, &signature)
        .map_err(|error| format!("self-check failed: {error}"))?;
    let destination = sig.map(Path::to_path_buf).unwrap_or_else(|| {
        let mut name = manifest.as_os_str().to_owned();
        name.push(".sig");
        PathBuf::from(name)
    });
    std::fs::write(
        &destination,
        format!("{}\n", STANDARD.encode(signature.to_bytes())),
    )
    .map_err(|error| format!("cannot write {}: {error}", destination.display()))?;
    Ok(destination.display().to_string())
}
