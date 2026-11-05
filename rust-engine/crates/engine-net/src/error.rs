//! Typed tunnel errors with stable string codes.
//!
//! No variant carries a password, passphrase or key bytes, so `Debug` and `Display` are safe to log.

use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Clone, Error)]
pub enum TunnelError {
    /// TCP connect, SSH handshake or transport failure before / outside authentication.
    #[error("cannot connect to SSH server {host}:{port}: {reason}")]
    Connect {
        host: String,
        port: u16,
        reason: String,
    },

    #[error("SSH authentication failed: {0}")]
    Auth(#[from] AuthError),

    /// The server key is in neither known_hosts file. The caller decides; to accept, pass `public_key` to
    /// [`trust_host_key`](crate::trust_host_key) and open the tunnel again.
    #[error("unknown SSH host key for {host}:{port} ({algorithm} {sha256_fingerprint})")]
    HostKeyUnknown {
        host: String,
        port: u16,
        algorithm: String,
        /// `SHA256:...` as printed by `ssh-keygen -lf`.
        sha256_fingerprint: String,
        /// The key in OpenSSH one-line form (`ssh-ed25519 AAAA...`); public data.
        public_key: String,
    },

    /// A key of the same type is recorded for this host and differs. Always refused; the user must remove
    /// the old entry deliberately.
    #[error(
        "SSH host key for {host}:{port} CHANGED ({algorithm} {sha256_fingerprint}); \
         known_hosts has a different key at {recorded_at}. Possible man-in-the-middle attack"
    )]
    HostKeyMismatch {
        host: String,
        port: u16,
        algorithm: String,
        sha256_fingerprint: String,
        /// `<file>:<line>` of the conflicting entry.
        recorded_at: String,
    },

    /// known_hosts has keys for this host, but none of the presented key's type: a downgrade attack looks
    /// exactly like this. Never offered for one-click trust; the user must remove the old entries deliberately.
    #[error(
        "SSH host {host}:{port} presented a {presented_algorithm} key ({sha256_fingerprint}) but known_hosts \
         only has {recorded_algorithms:?} for it (at {location}). Possible downgrade or man-in-the-middle attack"
    )]
    HostKeyTypeChanged {
        host: String,
        port: u16,
        presented_algorithm: String,
        sha256_fingerprint: String,
        recorded_algorithms: Vec<String>,
        /// `<file>:<line>` of the first recorded entry.
        location: String,
    },

    /// The presented key is marked `@revoked` in a known_hosts file.
    #[error("SSH host key for {host}:{port} is revoked ({algorithm} {sha256_fingerprint})")]
    HostKeyRevoked {
        host: String,
        port: u16,
        algorithm: String,
        sha256_fingerprint: String,
    },

    /// The server could not or would not open a `direct-tcpip` channel to the target.
    #[error("SSH server refused to forward to {host}:{port}: {reason}")]
    ForwardRefused {
        host: String,
        port: u16,
        reason: String,
    },

    /// The local client of a forward consumed nothing for `seconds` while the forward's buffer was full, so
    /// only that forward was closed.
    #[error("forward to {host}:{port} closed: the local client read nothing for {seconds}s")]
    ForwardStalled {
        host: String,
        port: u16,
        seconds: u64,
    },

    #[error("{operation} timed out after {seconds}s")]
    Timeout {
        operation: &'static str,
        seconds: u64,
    },

    /// The SSH session ended (dropped, killed, keepalive lost) or the tunnel handle was shut down.
    #[error("SSH tunnel closed")]
    Closed,

    #[error("invalid tunnel configuration: {0}")]
    Config(#[from] ConfigError),

    /// The proxy refused the connection, rejected the credentials, or did not speak its protocol.
    /// `proxy` is `SOCKS5 proxy host:port` / `HTTP proxy host:port`. Never carries the proxy password.
    #[error("{proxy}: {reason}")]
    Proxy { proxy: String, reason: String },

    /// A failure on one hop of a jump chain; `hop` says which (its text is `jump 1 of 2 user@host:port` or
    /// `bastion user@host:port`). Only produced when the spec has jump hosts, and never around the host key
    /// errors, which already carry the `host:port` they are about (so
    /// [`trust_host_key`](crate::trust_host_key) gets the right one). See [`TunnelError::hop`].
    #[error("{hop}: {source}")]
    Hop {
        hop: HopInfo,
        #[source]
        source: Box<TunnelError>,
    },
}

/// Which hop of the chain: position only (public data).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HopId {
    /// A jump host. `index` is 0-based in dial order; `of` is the number of jumps.
    Jump { index: usize, of: usize },
    /// The bastion, the last SSH server of the chain.
    Bastion,
}

/// The hop a [`TunnelError::Hop`] is about: its position and the public parts of its configuration, enough to
/// route a missing secret or user name to the right hop. Never a credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HopInfo {
    pub id: HopId,
    pub host: String,
    pub port: u16,
    pub user: String,
}

impl std::fmt::Display for HopInfo {
    /// `jump 1 of 2 user@host:port` / `bastion user@host:port`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.id {
            HopId::Jump { index, of } => write!(f, "jump {} of {of} ", index + 1)?,
            HopId::Bastion => f.write_str("bastion ")?,
        }
        write!(f, "{}@{}:{}", self.user, self.host, self.port)
    }
}

impl TunnelError {
    /// Stable machine-readable code for the Error Log and UI mapping.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Connect { .. } => "tunnel.connect",
            Self::Auth(_) => "tunnel.auth",
            Self::HostKeyUnknown { .. } => "tunnel.host_key_unknown",
            Self::HostKeyMismatch { .. }
            | Self::HostKeyTypeChanged { .. }
            | Self::HostKeyRevoked { .. } => "tunnel.host_key_mismatch",
            Self::ForwardRefused { .. } => "tunnel.forward_refused",
            Self::ForwardStalled { .. } => "tunnel.forward_stalled",
            Self::Timeout { .. } => "tunnel.timeout",
            Self::Closed => "tunnel.closed",
            Self::Config(_) => "tunnel.config",
            Self::Proxy { .. } => "tunnel.proxy",
            Self::Hop { source, .. } => source.code(),
        }
    }

    /// The hop this error is about, when the tunnel has jump hosts and the failure belongs to one hop.
    pub fn hop(&self) -> Option<&HopInfo> {
        match self {
            Self::Hop { hop, .. } => Some(hop),
            _ => None,
        }
    }

    /// The underlying failure, with any [`TunnelError::Hop`] context removed: match on this to react to
    /// `Auth(..)` or `Timeout { .. }` whether or not the tunnel has jump hosts.
    pub fn root(&self) -> &TunnelError {
        match self {
            Self::Hop { source, .. } => source.root(),
            other => other,
        }
    }
}

#[derive(Debug, Clone, Error)]
pub enum AuthError {
    #[error("the server rejected the {method} (still accepts: {remaining})")]
    Rejected {
        method: &'static str,
        remaining: String,
    },
    #[error("the private key {path} is encrypted and no passphrase was given")]
    PassphraseRequired { path: PathBuf },
    #[error("wrong passphrase for the private key {path}")]
    BadPassphrase { path: PathBuf },
    #[error("cannot read the private key {path}: {reason}")]
    KeyUnreadable { path: PathBuf, reason: String },
    #[error("the private key {path} is accessible by other users (mode {mode:04o}); run chmod 600 on it")]
    KeyPermissions { path: PathBuf, mode: u32 },
    #[error("ssh-agent is unavailable: {0}")]
    AgentUnavailable(String),
    #[error("ssh-agent has no identity the server accepts")]
    NoAgentIdentity,
    #[error("the server asked for a keyboard-interactive answer and none was provided")]
    PromptDeclined,
    #[error("the server does not offer any authentication method this tunnel can use")]
    NoUsableMethod,
}

#[derive(Debug, Clone, Error)]
pub enum ConfigError {
    #[error("{0}")]
    Invalid(String),
    #[error("ProxyCommand is not supported (it is never executed)")]
    ProxyCommandUnsupported,
    #[error("the ssh config uses {0}, which is not supported: host keys would be checked against the wrong record")]
    HostKeyDirectiveUnsupported(String),
    #[error("ssh config line {line}: {message}")]
    SshConfig { line: usize, message: String },
    #[error("cannot access {path}: {reason}")]
    Io { path: PathBuf, reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_stable() {
        let cases: Vec<(TunnelError, &str)> = vec![
            (
                TunnelError::Connect {
                    host: "h".into(),
                    port: 22,
                    reason: "r".into(),
                },
                "tunnel.connect",
            ),
            (AuthError::NoAgentIdentity.into(), "tunnel.auth"),
            (
                TunnelError::HostKeyUnknown {
                    host: "h".into(),
                    port: 22,
                    algorithm: "a".into(),
                    sha256_fingerprint: "f".into(),
                    public_key: "k".into(),
                },
                "tunnel.host_key_unknown",
            ),
            (
                TunnelError::HostKeyMismatch {
                    host: "h".into(),
                    port: 22,
                    algorithm: "a".into(),
                    sha256_fingerprint: "f".into(),
                    recorded_at: "x:1".into(),
                },
                "tunnel.host_key_mismatch",
            ),
            (
                TunnelError::HostKeyRevoked {
                    host: "h".into(),
                    port: 22,
                    algorithm: "a".into(),
                    sha256_fingerprint: "f".into(),
                },
                "tunnel.host_key_mismatch",
            ),
            (
                TunnelError::ForwardRefused {
                    host: "h".into(),
                    port: 1,
                    reason: "r".into(),
                },
                "tunnel.forward_refused",
            ),
            (
                TunnelError::ForwardStalled {
                    host: "h".into(),
                    port: 1,
                    seconds: 30,
                },
                "tunnel.forward_stalled",
            ),
            (
                TunnelError::Timeout {
                    operation: "connect",
                    seconds: 1,
                },
                "tunnel.timeout",
            ),
            (TunnelError::Closed, "tunnel.closed"),
            (
                TunnelError::HostKeyTypeChanged {
                    host: "h".into(),
                    port: 22,
                    presented_algorithm: "a".into(),
                    sha256_fingerprint: "f".into(),
                    recorded_algorithms: vec!["b".into()],
                    location: "x:1".into(),
                },
                "tunnel.host_key_mismatch",
            ),
            (
                TunnelError::Proxy {
                    proxy: "SOCKS5 proxy p:1".into(),
                    reason: "r".into(),
                },
                "tunnel.proxy",
            ),
            (
                TunnelError::Hop {
                    hop: HopInfo {
                        id: HopId::Jump { index: 0, of: 2 },
                        host: "h".into(),
                        port: 22,
                        user: "u".into(),
                    },
                    source: Box::new(AuthError::NoAgentIdentity.into()),
                },
                "tunnel.auth",
            ),
        ];
        for (error, code) in cases {
            assert_eq!(error.code(), code, "{error}");
        }
    }
}
