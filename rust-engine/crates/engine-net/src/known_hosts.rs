//! OpenSSH `known_hosts` reading, matching and (app-owned file only) writing.
//!
//! Supported: plain and `|1|` hashed host fields, comma lists, `*` / `?` wildcards with `!` negation,
//! `[host]:port`, several key types per host, `@revoked`. `@cert-authority` lines are parsed and ignored
//! (certificate host keys are not offered during key exchange, so a CA can never vouch for a host here).
//! A UTF-8 byte order mark on the first line is skipped.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use hmac::{Hmac, KeyInit, Mac};
use russh::keys::ssh_key::{Algorithm, HashAlg, PublicKey};
use sha1::Sha1;

use crate::error::{ConfigError, TunnelError};
use crate::glob::wildcard_match;
use crate::hostname;
use crate::spec::TunnelSpec;

/// Larger known_hosts files are refused instead of read into memory.
/// Large enough for a corporate `ssh_known_hosts`; the cap only stops a runaway file.
const MAX_FILE_BYTES: u64 = 32 << 20;

/// Serialises `trust_host_key` within the process (cross-process writers are not coordinated).
static WRITE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Marker {
    None,
    Revoked,
    CertAuthority,
}

#[derive(Debug, Clone)]
struct Pattern {
    negated: bool,
    glob: String,
}

#[derive(Debug, Clone)]
enum HostField {
    Hashed { salt: Vec<u8>, hash: Vec<u8> },
    Patterns(Vec<Pattern>),
}

impl HostField {
    fn parse(field: &str) -> Option<Self> {
        if let Some(rest) = field.strip_prefix("|1|") {
            let (salt, hash) = rest.split_once('|')?;
            return Some(Self::Hashed {
                salt: STANDARD.decode(salt).ok()?,
                hash: STANDARD.decode(hash).ok()?,
            });
        }
        let patterns = field
            .split(',')
            .filter(|p| !p.is_empty())
            .map(|p| match p.strip_prefix('!') {
                Some(glob) => Pattern {
                    negated: true,
                    glob: glob.to_owned(),
                },
                None => Pattern {
                    negated: false,
                    glob: p.to_owned(),
                },
            })
            .collect::<Vec<_>>();
        (!patterns.is_empty()).then_some(Self::Patterns(patterns))
    }

    /// `name` is `host` for port 22 and `[host]:port` otherwise, lowercase.
    fn matches(&self, name: &str) -> bool {
        match self {
            Self::Hashed { salt, hash } => Hmac::<Sha1>::new_from_slice(salt)
                .map(|mac| mac.chain_update(name).verify_slice(hash).is_ok())
                .unwrap_or(false),
            Self::Patterns(patterns) => {
                let hit = |negated: bool| {
                    patterns
                        .iter()
                        .filter(move |p| p.negated == negated)
                        .any(|p| wildcard_match(&p.glob, name))
                };
                hit(false) && !hit(true)
            }
        }
    }
}

#[derive(Debug, Clone)]
struct Entry {
    source: usize,
    line: usize,
    marker: Marker,
    hosts: HostField,
    key: PublicKey,
}

/// Result of checking a presented host key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostKeyStatus {
    Trusted,
    /// The host has no entry at all.
    Unknown,
    /// An entry of the same key type exists and differs; `recorded_at` is `<file>:<line>`.
    Changed {
        recorded_at: String,
    },
    /// The host has entries, but none of the presented key's type (possible downgrade).
    TypeChanged {
        recorded_algorithms: Vec<String>,
        location: String,
    },
    Revoked,
}

/// Entries from one or more known_hosts files.
#[derive(Debug, Default)]
pub struct KnownHosts {
    sources: Vec<String>,
    entries: Vec<Entry>,
}

/// Reads at most [`MAX_FILE_BYTES`]; a missing file is `Ok(None)`.
fn read_capped(path: &Path) -> Result<Option<String>, ConfigError> {
    let io_error = |reason: String| ConfigError::Io {
        path: path.to_owned(),
        reason,
    };
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_error(e.to_string())),
    };
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| io_error(e.to_string()))?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(io_error(format!(
            "larger than {} MiB",
            MAX_FILE_BYTES >> 20
        )));
    }
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

/// The name OpenSSH matches and hashes, for any host string (never fails: lookups fall back to lowercase).
fn match_name(host: &str, port: u16) -> String {
    match hostname::parse_host(host, true) {
        Ok(parsed) => parsed.known_hosts_name(port),
        Err(_) if port == 22 => host.to_ascii_lowercase(),
        Err(_) => format!("[{}]:{port}", host.to_ascii_lowercase()),
    }
}

impl KnownHosts {
    /// Adds the entries of `path`. A missing file is an empty file; an oversized one is an error.
    pub fn add_file(&mut self, path: &Path) -> Result<(), ConfigError> {
        if let Some(text) = read_capped(path)? {
            self.add_text(&text, &path.display().to_string());
        }
        Ok(())
    }

    /// Adds entries from text; `source` labels them in mismatch reports. Malformed lines and key types this
    /// build cannot parse are skipped, as OpenSSH does.
    pub fn add_text(&mut self, text: &str, source: &str) {
        let source_index = self.sources.len();
        self.sources.push(source.to_owned());
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let parsed = text.lines().enumerate().filter_map(|(index, line)| {
            parse_line(line).map(|(marker, hosts, key)| Entry {
                source: source_index,
                line: index + 1,
                marker,
                hosts,
                key,
            })
        });
        self.entries.extend(parsed);
    }

    fn matching<'a>(&'a self, host: &str, port: u16) -> impl Iterator<Item = &'a Entry> {
        let name = match_name(host, port);
        self.entries.iter().filter(move |e| e.hosts.matches(&name))
    }

    fn location(&self, entry: &Entry) -> String {
        format!("{}:{}", self.sources[entry.source], entry.line)
    }

    pub fn check(&self, host: &str, port: u16, key: &PublicKey) -> HostKeyStatus {
        let candidates: Vec<&Entry> = self.matching(host, port).collect();
        let same_key = |e: &&Entry| e.key.key_data() == key.key_data();
        // Revocation wins over any trusted line, wherever it appears.
        if candidates
            .iter()
            .any(|e| e.marker == Marker::Revoked && same_key(e))
        {
            return HostKeyStatus::Revoked;
        }
        if candidates
            .iter()
            .any(|e| e.marker == Marker::None && same_key(e))
        {
            return HostKeyStatus::Trusted;
        }
        // A host that is only covered by a certificate authority line is managed with host certificates,
        // which this crate does not verify. Presenting a bare key there must not look like a first contact.
        let certificate_authority = candidates
            .iter()
            .find(|e| e.marker == Marker::CertAuthority)
            .copied();
        let recorded: Vec<&Entry> = candidates
            .into_iter()
            .filter(|e| e.marker == Marker::None)
            .collect();
        if let Some(entry) = recorded
            .iter()
            .find(|e| e.key.algorithm() == key.algorithm())
        {
            return HostKeyStatus::Changed {
                recorded_at: self.location(entry),
            };
        }
        match recorded.first() {
            Some(first) => {
                let mut recorded_algorithms: Vec<String> = Vec::new();
                for entry in &recorded {
                    let name = entry.key.algorithm().to_string();
                    if !recorded_algorithms.contains(&name) {
                        recorded_algorithms.push(name);
                    }
                }
                HostKeyStatus::TypeChanged {
                    recorded_algorithms,
                    location: self.location(first),
                }
            }
            None => match certificate_authority {
                Some(entry) => HostKeyStatus::TypeChanged {
                    recorded_algorithms: vec![
                        "host certificate (@cert-authority, not supported)".into()
                    ],
                    location: self.location(entry),
                },
                None => HostKeyStatus::Unknown,
            },
        }
    }

    /// Key algorithms already trusted for this host, so negotiation can prefer them (OpenSSH does the same).
    pub fn known_algorithms(&self, host: &str, port: u16) -> Vec<Algorithm> {
        let mut algorithms: Vec<Algorithm> = Vec::new();
        for entry in self
            .matching(host, port)
            .filter(|e| e.marker == Marker::None)
        {
            let algorithm = entry.key.algorithm();
            if !algorithms.contains(&algorithm) {
                algorithms.push(algorithm);
            }
        }
        algorithms
    }
}

fn parse_line(line: &str) -> Option<(Marker, HostField, PublicKey)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let mut fields = line.split_ascii_whitespace();
    let mut first = fields.next()?;
    let marker = match first {
        "@revoked" => Marker::Revoked,
        "@cert-authority" => Marker::CertAuthority,
        _ if first.starts_with('@') => return None,
        _ => Marker::None,
    };
    if marker != Marker::None {
        first = fields.next()?;
    }
    let hosts = HostField::parse(first)?;
    let algorithm = fields.next()?;
    let blob = fields.next()?;
    let key = PublicKey::from_openssh(&format!("{algorithm} {blob}")).ok()?;
    Some((marker, hosts, key))
}

/// `|1|salt|hash key` line for the app-owned file (no trailing newline). Hashing means the file never
/// contains host patterns, only opaque digests of one exact name.
fn format_entry(name: &str, key: &PublicKey) -> Result<String, ConfigError> {
    let mut salt = [0u8; 20];
    getrandom::fill(&mut salt).map_err(|e| {
        ConfigError::Invalid(format!(
            "no secure random source for the known_hosts salt: {e}"
        ))
    })?;
    let hash = Hmac::<Sha1>::new_from_slice(&salt)
        .map_err(|e| ConfigError::Invalid(e.to_string()))?
        .chain_update(name)
        .finalize()
        .into_bytes();
    let openssh = key
        .to_openssh()
        .map_err(|e| ConfigError::Invalid(format!("cannot encode the host key: {e}")))?;
    Ok(format!(
        "|1|{}|{} {}",
        STANDARD.encode(salt),
        STANDARD.encode(hash),
        openssh
    ))
}

/// Records `public_key` (the `public_key` field of [`TunnelError::HostKeyUnknown`]) as trusted for
/// `host:port` in the app-owned `spec.app_known_hosts`. Never touches any other file.
///
/// Pass the `host` and `port` **of the error**: with jump hosts the unknown key may belong to any hop, not to
/// `spec.host`. `spec` only supplies the file locations.
///
/// Refuses (typed errors, nothing written) when the host name is not a plain RFC 1123 name or IP literal, when
/// any known_hosts file (app, user or system) already holds a different key of the same type
/// (`HostKeyMismatch`), keys of other types only (`HostKeyTypeChanged`), or revokes this key
/// (`HostKeyRevoked`): trusting can never replace or extend an existing record. Idempotent when the key is
/// already trusted. The entry is hashed (`|1|salt|hash`). The file is replaced atomically (random 0600 temp
/// file in the same directory, fsync, rename, directory fsync) under a process-wide lock.
pub fn trust_host_key(
    spec: &TunnelSpec,
    host: &str,
    port: u16,
    public_key: &str,
) -> Result<(), TunnelError> {
    let parsed_host = hostname::parse_host(host, false).map_err(ConfigError::Invalid)?;
    if port == 0 {
        return Err(ConfigError::Invalid("SSH port must not be 0".into()).into());
    }
    let key = PublicKey::from_openssh(public_key)
        .map_err(|e| ConfigError::Invalid(format!("not an OpenSSH public key: {e}")))?;
    let app_path = spec.app_known_hosts.as_path();

    let _guard = WRITE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let existing = read_capped(app_path)?.unwrap_or_default();
    let mut hosts = KnownHosts::default();
    for read_only in [&spec.user_known_hosts, &spec.system_known_hosts]
        .into_iter()
        .flatten()
    {
        hosts.add_file(read_only)?;
    }
    hosts.add_text(&existing, &app_path.display().to_string());

    let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
    let algorithm = key.algorithm().to_string();
    let name = host.to_owned();
    match hosts.check(&name, port, &key) {
        HostKeyStatus::Trusted => return Ok(()),
        HostKeyStatus::Changed { recorded_at } => {
            return Err(TunnelError::HostKeyMismatch {
                host: name,
                port,
                algorithm,
                sha256_fingerprint: fingerprint,
                recorded_at,
            })
        }
        HostKeyStatus::TypeChanged {
            recorded_algorithms,
            location,
        } => {
            return Err(TunnelError::HostKeyTypeChanged {
                host: name,
                port,
                presented_algorithm: algorithm,
                sha256_fingerprint: fingerprint,
                recorded_algorithms,
                location,
            })
        }
        HostKeyStatus::Revoked => {
            return Err(TunnelError::HostKeyRevoked {
                host: name,
                port,
                algorithm,
                sha256_fingerprint: fingerprint,
            })
        }
        HostKeyStatus::Unknown => {}
    }

    let mut contents = existing;
    if !contents.is_empty() && !contents.ends_with('\n') {
        contents.push('\n');
    }
    contents.push_str(&format_entry(&parsed_host.known_hosts_name(port), &key)?);
    contents.push('\n');
    write_atomically(app_path, contents.as_bytes()).map_err(|e| {
        ConfigError::Io {
            path: app_path.to_owned(),
            reason: e.to_string(),
        }
        .into()
    })
}

fn write_atomically(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let mut nonce = [0u8; 8];
    getrandom::fill(&mut nonce).map_err(|e| std::io::Error::other(e.to_string()))?;
    let mut temp_name = path.file_name().unwrap_or_default().to_os_string();
    temp_name.push(format!(
        ".tmp-{}",
        nonce.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ));
    let temp: PathBuf = dir.join(temp_name);

    // create_new = O_EXCL: never reuses or follows a pre-planted file or symlink.
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let result = options
        .open(&temp)
        .and_then(|mut file| file.write_all(contents).and_then(|()| file.sync_all()))
        .and_then(|()| fs::rename(&temp, path))
        .and_then(|()| sync_directory(dir));
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Makes the rename durable. Directories cannot be opened for fsync on Windows.
fn sync_directory(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        fs::File::open(dir)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::{Auth, Target};

    // Throwaway public keys generated for these tests (no private half exists anywhere).
    const ED_A: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPVAm2qgR75sGnFvHErAwTW5xXN4LELTJAkw9R1hHUux";
    const ED_B: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOB2eAzhXfG/iR6rH2OG/O394S9h+IaW0uOSd5mgHD0p";
    const ECDSA: &str = "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBNWYMdV2R0NMmQwxbRWzo/kFiP8PVxnNjNPZH9mG4sazYYcp+60RfbceVaAMAYzjUudg793NHqNoZTE379IGB3k=";

    fn key(line: &str) -> PublicKey {
        PublicKey::from_openssh(line).expect("test key parses")
    }

    fn hashed_field(name: &str) -> String {
        let salt = [7u8; 20];
        let mac = Hmac::<Sha1>::new_from_slice(&salt)
            .unwrap()
            .chain_update(name)
            .finalize()
            .into_bytes();
        format!("|1|{}|{}", STANDARD.encode(salt), STANDARD.encode(mac))
    }

    fn hosts(text: &str) -> KnownHosts {
        let mut hosts = KnownHosts::default();
        hosts.add_text(text, "known_hosts");
        hosts
    }

    /// A spec that only matters for its known_hosts settings.
    fn spec_in(dir: &Path, host: &str, port: u16) -> TunnelSpec {
        let mut spec = TunnelSpec::new(
            host,
            "u",
            Auth::KeyboardInteractive,
            dir.join("app_known_hosts"),
        );
        spec.port = port;
        spec.user_known_hosts = None;
        spec.system_known_hosts = None;
        spec.targets.push(Target::new("db", 1));
        spec
    }

    #[test]
    fn plain_entry_trusts_the_same_key() {
        let h = hosts(&format!("bastion.example.com {ED_A}\n"));
        assert_eq!(
            h.check("bastion.example.com", 22, &key(ED_A)),
            HostKeyStatus::Trusted
        );
        assert_eq!(
            h.check("BASTION.example.com", 22, &key(ED_A)),
            HostKeyStatus::Trusted
        );
        assert_eq!(
            h.check("other.example.com", 22, &key(ED_A)),
            HostKeyStatus::Unknown
        );
    }

    #[test]
    fn non_default_port_uses_bracket_form() {
        let h = hosts(&format!(
            "[bastion.example.com]:2222 {ED_A}\nbastion.example.com {ED_B}\n"
        ));
        assert_eq!(
            h.check("bastion.example.com", 2222, &key(ED_A)),
            HostKeyStatus::Trusted
        );
        // The port-22 entry must not vouch for port 2222, nor the other way round.
        assert_eq!(
            h.check("bastion.example.com", 2222, &key(ED_B)),
            HostKeyStatus::Changed {
                recorded_at: "known_hosts:1".into()
            }
        );
        assert_eq!(
            h.check("bastion.example.com", 22, &key(ED_A)),
            HostKeyStatus::Changed {
                recorded_at: "known_hosts:2".into()
            }
        );
    }

    #[test]
    fn ipv6_literal_plain_and_bracketed_port() {
        let h = hosts(&format!("2001:db8::1 {ED_A}\n[2001:db8::1]:2222 {ED_B}\n"));
        assert_eq!(
            h.check("2001:db8::1", 22, &key(ED_A)),
            HostKeyStatus::Trusted
        );
        // Non-canonical spelling of the same address matches too.
        assert_eq!(
            h.check("2001:0DB8:0:0:0:0:0:1", 22, &key(ED_A)),
            HostKeyStatus::Trusted
        );
        assert_eq!(
            h.check("2001:db8::1", 2222, &key(ED_B)),
            HostKeyStatus::Trusted
        );
        assert_eq!(
            h.check("2001:db8::1", 2222, &key(ED_A)),
            HostKeyStatus::Changed {
                recorded_at: "known_hosts:2".into()
            }
        );
    }

    #[test]
    fn hashed_entries_match_plain_and_bracketed_names() {
        let text = format!(
            "{} {ED_A}\n{} {ED_B}\n",
            hashed_field("bastion.example.com"),
            hashed_field("[bastion.example.com]:2200")
        );
        let h = hosts(&text);
        assert_eq!(
            h.check("bastion.example.com", 22, &key(ED_A)),
            HostKeyStatus::Trusted
        );
        assert_eq!(
            h.check("bastion.example.com", 2200, &key(ED_B)),
            HostKeyStatus::Trusted
        );
        assert_eq!(
            h.check("bastion.example.com", 2201, &key(ED_B)),
            HostKeyStatus::Unknown
        );
        assert_eq!(
            h.check("bastion.example.org", 22, &key(ED_A)),
            HostKeyStatus::Unknown
        );
    }

    #[test]
    fn changed_key_of_the_same_type_is_reported_with_its_line() {
        let h = hosts(&format!("# comment\n\nbastion.example.com {ED_A}\n"));
        assert_eq!(
            h.check("bastion.example.com", 22, &key(ED_B)),
            HostKeyStatus::Changed {
                recorded_at: "known_hosts:3".into()
            }
        );
    }

    #[test]
    fn recorded_ed25519_host_presenting_only_rsa_is_not_plain_unknown() {
        // The "other type" key here is ecdsa; the rule is about any type the host has no record of.
        let h = hosts(&format!("bastion.example.com {ED_A}\n"));
        assert_eq!(
            h.check("bastion.example.com", 22, &key(ECDSA)),
            HostKeyStatus::TypeChanged {
                recorded_algorithms: vec!["ssh-ed25519".into()],
                location: "known_hosts:1".into()
            }
        );
        // A host with no record at all is still plain Unknown.
        assert_eq!(
            h.check("other.example.com", 22, &key(ECDSA)),
            HostKeyStatus::Unknown
        );
    }

    #[test]
    fn multiple_key_types_for_one_host() {
        let h = hosts(&format!(
            "bastion.example.com {ED_A}\nbastion.example.com {ECDSA}\n"
        ));
        assert_eq!(
            h.check("bastion.example.com", 22, &key(ECDSA)),
            HostKeyStatus::Trusted
        );
        assert_eq!(
            h.check("bastion.example.com", 22, &key(ED_A)),
            HostKeyStatus::Trusted
        );
        assert_eq!(h.known_algorithms("bastion.example.com", 22).len(), 2);
    }

    #[test]
    fn revoked_wins_even_when_a_trusted_line_exists() {
        let h = hosts(&format!(
            "bastion.example.com {ED_A}\n@revoked bastion.example.com {ED_A}\n"
        ));
        assert_eq!(
            h.check("bastion.example.com", 22, &key(ED_A)),
            HostKeyStatus::Revoked
        );
        let h = hosts(&format!("@revoked * {ED_A}\nbastion.example.com {ED_B}\n"));
        assert_eq!(
            h.check("anything.example.com", 22, &key(ED_A)),
            HostKeyStatus::Revoked
        );
        assert_eq!(
            h.check("bastion.example.com", 22, &key(ED_B)),
            HostKeyStatus::Trusted
        );
    }

    #[test]
    fn known_hosts_bom_first_line_revoked() {
        let h = hosts(&format!(
            "\u{feff}@revoked bastion.example.com {ED_A}\nbastion.example.com {ED_A}\n"
        ));
        assert_eq!(
            h.check("bastion.example.com", 22, &key(ED_A)),
            HostKeyStatus::Revoked
        );
        let h = hosts(&format!("\u{feff}bastion.example.com {ED_A}\n"));
        assert_eq!(
            h.check("bastion.example.com", 22, &key(ED_A)),
            HostKeyStatus::Trusted
        );
    }

    #[test]
    fn cert_authority_only_host_is_not_one_click() {
        let h = hosts(&format!("@cert-authority *.example.com {ED_A}\n"));
        // Never trusted by the CA line, and not a first contact either: the host is certificate-managed.
        let status = h.check("bastion.example.com", 22, &key(ED_A));
        assert!(
            matches!(&status, HostKeyStatus::TypeChanged { recorded_algorithms, .. }
                if recorded_algorithms.iter().any(|a| a.contains("cert-authority"))),
            "{status:?}"
        );
        // A host the CA line does not cover is still a plain first contact.
        assert_eq!(
            h.check("bastion.other.test", 22, &key(ED_A)),
            HostKeyStatus::Unknown
        );
    }

    #[test]
    fn wildcards_negation_and_comma_lists() {
        let h = hosts(&format!(
            "*.example.com,!secret.example.com,10.0.0.? {ED_A}\n"
        ));
        assert_eq!(
            h.check("a.example.com", 22, &key(ED_A)),
            HostKeyStatus::Trusted
        );
        assert_eq!(
            h.check("secret.example.com", 22, &key(ED_A)),
            HostKeyStatus::Unknown
        );
        assert_eq!(h.check("10.0.0.7", 22, &key(ED_A)), HostKeyStatus::Trusted);
    }

    #[test]
    fn malformed_lines_and_unknown_markers_are_skipped() {
        let h = hosts(&format!(
            "justonefield\nhost ssh-ed25519 not-base64!!\n@weird host {ED_A}\n|1|only-one-part {ED_A}\nok.example.com {ED_A}\n"
        ));
        assert_eq!(
            h.check("ok.example.com", 22, &key(ED_A)),
            HostKeyStatus::Trusted
        );
        assert_eq!(h.check("host", 22, &key(ED_A)), HostKeyStatus::Unknown);
    }

    #[test]
    fn trust_writes_hashed_entries_atomically_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let spec = spec_in(&dir.path().join("nested"), "Bastion.example.com", 2222);
        trust_host_key(&spec, &spec.host, spec.port, ED_A).unwrap();
        trust_host_key(&spec, &spec.host, spec.port, ED_A).unwrap();
        let other = spec_in(&dir.path().join("nested"), "other.example.com", 22);
        trust_host_key(&other, &other.host, other.port, ED_B).unwrap();

        let text = fs::read_to_string(&spec.app_known_hosts).unwrap();
        assert_eq!(text.lines().count(), 2, "{text}");
        assert!(
            text.lines().all(|l| l.starts_with("|1|")),
            "only hashed entries: {text}"
        );
        assert!(
            !text.contains("example.com") && !text.contains("2222"),
            "no host names in the file: {text}"
        );
        // Hashed round trip.
        let mut reread = KnownHosts::default();
        reread.add_file(&spec.app_known_hosts).unwrap();
        assert_eq!(
            reread.check("bastion.example.com", 2222, &key(ED_A)),
            HostKeyStatus::Trusted
        );
        assert_eq!(
            reread.check("other.example.com", 22, &key(ED_B)),
            HostKeyStatus::Trusted
        );
        assert_eq!(
            reread.check("bastion.example.com", 22, &key(ED_A)),
            HostKeyStatus::Unknown
        );
        assert!(fs::read_dir(spec.app_known_hosts.parent().unwrap())
            .unwrap()
            .all(|e| !e.unwrap().file_name().to_string_lossy().contains(".tmp-")));
    }

    #[test]
    fn trust_host_key_rejects_pattern_metacharacters_in_host() {
        let dir = tempfile::tempdir().unwrap();
        for bad in [
            "*",
            "*,x.attacker.tld",
            "a?b",
            "!a",
            "a,b",
            "[::1]",
            "[a]:22",
            "a|b",
            "|1|x|y",
            "a#b",
            "a@b",
            "a b",
            " a",
            "a\tb",
            "a\nb",
            "",
        ] {
            let mut spec = spec_in(dir.path(), "placeholder", 22);
            spec.host = bad.into();
            let err = trust_host_key(&spec, &spec.host, spec.port, ED_A).unwrap_err();
            assert_eq!(err.code(), "tunnel.config", "{bad:?}: {err}");
        }
        assert!(
            !dir.path().join("app_known_hosts").exists(),
            "nothing may be written"
        );
    }

    #[test]
    fn trust_refuses_to_replace_a_recorded_key() {
        let dir = tempfile::tempdir().unwrap();
        let spec = spec_in(dir.path(), "bastion.example.com", 22);
        trust_host_key(&spec, &spec.host, spec.port, ED_A).unwrap();
        let before = fs::read_to_string(&spec.app_known_hosts).unwrap();
        let err = trust_host_key(&spec, &spec.host, spec.port, ED_B).unwrap_err();
        assert!(matches!(err, TunnelError::HostKeyMismatch { .. }), "{err}");
        assert_eq!(fs::read_to_string(&spec.app_known_hosts).unwrap(), before);
    }

    #[test]
    fn trust_refuses_a_key_of_another_type_for_a_recorded_host() {
        let dir = tempfile::tempdir().unwrap();
        let spec = spec_in(dir.path(), "bastion.example.com", 22);
        trust_host_key(&spec, &spec.host, spec.port, ED_A).unwrap();
        let before = fs::read_to_string(&spec.app_known_hosts).unwrap();
        let err = trust_host_key(&spec, &spec.host, spec.port, ECDSA).unwrap_err();
        assert!(
            matches!(err, TunnelError::HostKeyTypeChanged { .. }),
            "{err}"
        );
        assert_eq!(err.code(), "tunnel.host_key_mismatch");
        assert_eq!(fs::read_to_string(&spec.app_known_hosts).unwrap(), before);
    }

    #[test]
    fn trust_checks_the_user_and_system_files_too() {
        let dir = tempfile::tempdir().unwrap();
        let mut spec = spec_in(dir.path(), "bastion.example.com", 22);
        let user = dir.path().join("user_kh");
        let system = dir.path().join("system_kh");
        fs::write(&user, format!("bastion.example.com {ED_A}\n")).unwrap();
        fs::write(&system, format!("@revoked bastion.example.com {ECDSA}\n")).unwrap();
        spec.user_known_hosts = Some(user.clone());
        spec.system_known_hosts = Some(system);
        assert!(matches!(
            trust_host_key(&spec, &spec.host, spec.port, ED_B),
            Err(TunnelError::HostKeyMismatch { .. })
        ));
        assert!(
            trust_host_key(&spec, &spec.host, spec.port, ED_A).is_ok(),
            "already trusted in the user file: nothing to write"
        );
        assert!(!spec.app_known_hosts.exists());
        assert_eq!(
            fs::read_to_string(&user).unwrap(),
            format!("bastion.example.com {ED_A}\n")
        );
    }

    #[test]
    fn trust_host_key_refuses_revoked_key() {
        let dir = tempfile::tempdir().unwrap();
        let mut spec = spec_in(dir.path(), "bastion.example.com", 22);
        let user = dir.path().join("user_kh");
        fs::write(&user, format!("@revoked bastion.example.com {ED_A}\n")).unwrap();
        spec.user_known_hosts = Some(user);
        let err = trust_host_key(&spec, &spec.host, spec.port, ED_A).unwrap_err();
        assert!(matches!(err, TunnelError::HostKeyRevoked { .. }), "{err}");
        assert!(!spec.app_known_hosts.exists());
    }

    #[test]
    fn trust_host_key_concurrent_writers_keep_both_entries() {
        let dir = tempfile::tempdir().unwrap();
        let handles: Vec<_> = (0..16)
            .map(|i| {
                let spec = spec_in(dir.path(), &format!("host{i}.example.com"), 22);
                std::thread::spawn(move || {
                    trust_host_key(
                        &spec,
                        &spec.host,
                        spec.port,
                        if i % 2 == 0 { ED_A } else { ED_B },
                    )
                    .unwrap()
                })
            })
            .collect();
        handles.into_iter().for_each(|h| h.join().unwrap());
        let mut reread = KnownHosts::default();
        reread
            .add_file(&dir.path().join("app_known_hosts"))
            .unwrap();
        for i in 0..16 {
            let expected = key(if i % 2 == 0 { ED_A } else { ED_B });
            assert_eq!(
                reread.check(&format!("host{i}.example.com"), 22, &expected),
                HostKeyStatus::Trusted,
                "{i}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn trust_host_key_file_mode_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let spec = spec_in(dir.path(), "bastion.example.com", 22);
        trust_host_key(&spec, &spec.host, spec.port, ED_A).unwrap();
        let mode = fs::metadata(&spec.app_known_hosts)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn trust_keeps_an_existing_file_without_trailing_newline_intact() {
        let dir = tempfile::tempdir().unwrap();
        let spec = spec_in(dir.path(), "second.example.com", 22);
        fs::write(&spec.app_known_hosts, format!("first.example.com {ED_A}")).unwrap();
        trust_host_key(&spec, &spec.host, spec.port, ED_B).unwrap();
        let text = fs::read_to_string(&spec.app_known_hosts).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(text.starts_with("first.example.com ssh-ed25519"));
    }

    #[test]
    fn trust_rejects_garbage_keys() {
        let dir = tempfile::tempdir().unwrap();
        let err = trust_host_key(
            &spec_in(dir.path(), "h.example.com", 22),
            "h.example.com",
            22,
            "not a key",
        )
        .unwrap_err();
        assert_eq!(err.code(), "tunnel.config");
    }

    #[test]
    fn missing_file_is_an_empty_store_and_oversized_files_are_refused() {
        let mut h = KnownHosts::default();
        h.add_file(Path::new("/nonexistent-dir-for-test/known_hosts"))
            .unwrap();
        assert_eq!(h.check("a", 22, &key(ED_A)), HostKeyStatus::Unknown);

        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big");
        let file = fs::File::create(&big).unwrap();
        file.set_len(MAX_FILE_BYTES + 1).unwrap();
        assert!(matches!(
            KnownHosts::default().add_file(&big),
            Err(ConfigError::Io { .. })
        ));
    }
}
