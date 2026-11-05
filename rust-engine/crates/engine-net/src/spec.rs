//! What the caller asks for: where the bastion is, how to authenticate, what to forward to.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::error::{ConfigError, HopId, HopInfo, TunnelError};
use crate::hostname;
use crate::paths;
use crate::proxy::ProxySpec;
use crate::secret::Secret;
use crate::ssh_config::{ResolvedChain, ResolvedHost};

/// Most jump hosts in front of the bastion.
pub const MAX_JUMPS: usize = 8;

/// Default cap on simultaneously forwarded connections per tunnel.
pub const DEFAULT_MAX_CONCURRENT_FORWARDS: usize = 256;
/// Default per-forward buffer (remote data waiting for a slow local reader).
pub const DEFAULT_FORWARD_BUFFER_BYTES: usize = 32 << 20;
/// Default per-tunnel total of those buffers.
pub const DEFAULT_TUNNEL_BUFFER_BYTES: usize = 256 << 20;

/// A `host:port` reachable *from the bastion* (the database endpoint).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Target {
    pub host: String,
    pub port: u16,
}

impl Target {
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
        }
    }
}

/// An SSH server the connection passes through on its way to the bastion. The first jump is dialled first;
/// each next SSH session runs inside a `direct-tcpip` channel of the one before it.
#[derive(Debug, Clone)]
pub struct Jump {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: Auth,
}

impl Jump {
    /// Port 22.
    pub fn new(host: impl Into<String>, user: impl Into<String>, auth: Auth) -> Self {
        Self {
            host: host.into(),
            port: 22,
            user: user.into(),
            auth,
        }
    }
}

/// How to prove identity to the bastion. Secrets are [`Secret`]s: redacted in `Debug`, zeroized on drop.
#[derive(Debug, Clone)]
pub enum Auth {
    Password(Secret),
    /// Private key file (OpenSSH or PEM; ed25519, ecdsa, rsa). `passphrase` is needed only for encrypted keys.
    KeyFile {
        path: PathBuf,
        passphrase: Option<Secret>,
    },
    /// ssh-agent. `socket: None` uses `SSH_AUTH_SOCK` on Unix and the OpenSSH named pipe, then Pageant, on
    /// Windows. `Some(path)` is a Unix socket path / Windows named-pipe name.
    Agent {
        socket: Option<PathBuf>,
    },
    /// Every challenge is answered through [`Prompts`].
    KeyboardInteractive,
}

/// Upper bounds for each phase. Prompt think-time is bounded separately and never charged to `auth`.
#[derive(Debug, Clone)]
pub struct Timeouts {
    /// TCP connect plus the SSH handshake (key exchange and host key check).
    pub connect: Duration,
    /// Each authentication round trip.
    pub auth: Duration,
    /// Opening one `direct-tcpip` channel to a target.
    pub forward: Duration,
    /// Waiting for the user to answer a keyboard-interactive prompt.
    pub prompt: Duration,
    /// SSH keepalive interval; the session is declared dead after three unanswered keepalives.
    pub keepalive: Duration,
    /// A forward whose local client consumes **no** data at all for this long while its buffer is full is
    /// closed with `tunnel.forward_stalled` (the deadlock breaker for an abandoned reader). A slow but live
    /// reader never trips it: any progress restarts the clock.
    pub forward_stall: Duration,
    /// How long the SSH session may take nothing from its work queue before the tunnel is declared dead and
    /// closed (the caller reopens it). This catches a session that looks alive but no longer sends anything.
    /// It also ends an upload whose remote consumer accepts no data at all for this long (a `COPY` waiting
    /// on a lock), which plain TCP would wait out: keep it generous.
    pub outbound_liveness: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(10),
            auth: Duration::from_secs(20),
            forward: Duration::from_secs(10),
            prompt: Duration::from_secs(300),
            keepalive: Duration::from_secs(30),
            forward_stall: Duration::from_secs(30),
            outbound_liveness: Duration::from_secs(300),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TunnelSpec {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: Auth,
    /// One local listener is created per distinct target; all share one SSH session.
    pub targets: Vec<Target>,
    /// Jump hosts in dial order (at most [`MAX_JUMPS`]); empty connects to the bastion directly. Every hop is
    /// verified against known_hosts before any credential is sent to it, and authenticates with its own `auth`.
    pub jumps: Vec<Jump>,
    /// A proxy that carries the TCP connection to the first hop (the first jump, or the bastion).
    pub proxy: Option<ProxySpec>,
    /// App-owned known_hosts (read, and written by [`trust_host_key`](crate::trust_host_key)).
    pub app_known_hosts: PathBuf,
    /// The user's known_hosts, read-only; `None` skips it. [`TunnelSpec::new`] defaults to `~/.ssh/known_hosts`.
    pub user_known_hosts: Option<PathBuf>,
    /// The machine-wide known_hosts, read-only; `None` skips it. Defaults to `/etc/ssh/ssh_known_hosts`
    /// (`%ProgramData%\ssh\ssh_known_hosts` on Windows).
    pub system_known_hosts: Option<PathBuf>,
    pub timeouts: Timeouts,
    /// Allow `diffie-hellman-group14-sha1` key exchange and SHA-1 `ssh-rsa` signatures, for old appliances.
    /// Off by default.
    pub allow_legacy_algorithms: bool,
    /// Connections beyond this many at once are refused with `ForwardRefused` (the rest keep working).
    pub max_concurrent_forwards: usize,
    /// Remote data one forward may buffer in front of its local socket (default 32 MiB). At the limit the
    /// forward stops draining its SSH channel (backpressure; see [`Timeouts::forward_stall`]). Memory bound of
    /// a tunnel: `tunnel_buffer_bytes` plus, per busy forward, up to 256 SSH packets (8 MiB) queued inside
    /// the SSH library.
    pub forward_buffer_bytes: usize,
    /// The same, summed over all forwards of the tunnel (default 256 MiB).
    pub tunnel_buffer_bytes: usize,
}

impl TunnelSpec {
    pub fn new(
        host: impl Into<String>,
        user: impl Into<String>,
        auth: Auth,
        app_known_hosts: PathBuf,
    ) -> Self {
        Self {
            host: host.into(),
            port: 22,
            user: user.into(),
            auth,
            targets: Vec::new(),
            jumps: Vec::new(),
            proxy: None,
            app_known_hosts,
            user_known_hosts: paths::default_user_known_hosts(),
            system_known_hosts: paths::default_system_known_hosts(),
            timeouts: Timeouts::default(),
            allow_legacy_algorithms: false,
            max_concurrent_forwards: DEFAULT_MAX_CONCURRENT_FORWARDS,
            forward_buffer_bytes: DEFAULT_FORWARD_BUFFER_BYTES,
            tunnel_buffer_bytes: DEFAULT_TUNNEL_BUFFER_BYTES,
        }
    }

    /// Builds a spec from a resolved `~/.ssh/config` host and its `ProxyJump` chain
    /// ([`SshConfig::resolve_chain`](crate::SshConfig::resolve_chain)). Fails typed when the config would need
    /// something this crate never does: `ProxyCommand` (never executed), on the target or on any jump.
    /// `HostKeyAlias`, `UserKnownHostsFile` and `GlobalKnownHostsFile` are refused too (they change which
    /// record a host key is checked against). The remaining unsupported directives (`IdentityAgent`, `Match`,
    /// `Include`) are left in the hosts' `unsupported` lists for the caller to surface.
    ///
    /// `auth` is the bastion's; with `None` its first `IdentityFile` is used, else the agent. Every jump gets
    /// the same default and the caller overrides it by assigning `spec.jumps[i].auth`. A jump without a
    /// `User` gets an empty one, which [`TunnelSpec::validate`] refuses until the caller fills it in.
    pub fn from_ssh_config(
        chain: &ResolvedChain,
        auth: Option<Auth>,
        app_known_hosts: PathBuf,
    ) -> Result<Self, TunnelError> {
        let default_auth = |resolved: &ResolvedHost| {
            resolved
                .identity_files
                .first()
                .map_or(Auth::Agent { socket: None }, |path| Auth::KeyFile {
                    path: path.clone(),
                    passphrase: None,
                })
        };
        // These change which record a host key is checked against; ignoring them would turn a recorded
        // host into an unknown one.
        const HOST_KEY_DIRECTIVES: [&str; 3] =
            ["HostKeyAlias", "UserKnownHostsFile", "GlobalKnownHostsFile"];
        for resolved in chain.jumps.iter().chain([&chain.target]) {
            if resolved
                .unsupported
                .iter()
                .any(|d| d.keyword.eq_ignore_ascii_case("ProxyCommand"))
            {
                return Err(ConfigError::ProxyCommandUnsupported.into());
            }
            if let Some(directive) = resolved.unsupported.iter().find(|d| {
                HOST_KEY_DIRECTIVES
                    .iter()
                    .any(|name| d.keyword.eq_ignore_ascii_case(name))
            }) {
                return Err(
                    ConfigError::HostKeyDirectiveUnsupported(directive.keyword.clone()).into(),
                );
            }
        }
        let target = &chain.target;
        let auth = auth.unwrap_or_else(|| default_auth(target));
        let user = target.user.clone().unwrap_or_default();
        let mut spec = Self::new(target.host_name.clone(), user, auth, app_known_hosts);
        spec.port = target.port;
        spec.jumps = chain
            .jumps
            .iter()
            .map(|jump| Jump {
                host: jump.host_name.clone(),
                port: jump.port,
                user: jump.user.clone().unwrap_or_default(),
                auth: default_auth(jump),
            })
            .collect();
        Ok(spec)
    }

    /// Every SSH hop in dial order: the jumps, then the bastion.
    pub(crate) fn hops(&self) -> Vec<Hop<'_>> {
        let of = self.jumps.len();
        let named = |id: HopId| (of > 0).then_some(id);
        let jump_hops = self.jumps.iter().enumerate().map(|(i, jump)| Hop {
            host: &jump.host,
            port: jump.port,
            user: &jump.user,
            auth: &jump.auth,
            id: named(HopId::Jump { index: i, of }),
        });
        let bastion = Hop {
            host: &self.host,
            port: self.port,
            user: &self.user,
            auth: &self.auth,
            id: named(HopId::Bastion),
        };
        jump_hops.chain([bastion]).collect()
    }

    /// Checks everything that can be checked without the network.
    pub fn validate(&self) -> Result<(), TunnelError> {
        require(
            self.jumps.len() <= MAX_JUMPS,
            &format!("at most {MAX_JUMPS} jump hosts are supported"),
        )?;
        let hops = self.hops();
        for hop in &hops {
            hop.validate()?;
        }
        // A hop that appears twice would loop through itself; the chain is refused before anything connects.
        for (index, hop) in hops.iter().enumerate() {
            if let Some(first) = hops[..index]
                .iter()
                .find(|other| other.port == hop.port && other.host.eq_ignore_ascii_case(hop.host))
            {
                return Err(ConfigError::Invalid(format!(
                    "{} repeats {}:{} (already used by {}): a jump chain must not visit a host twice",
                    hop.name(),
                    hop.host,
                    hop.port,
                    first.name()
                ))
                .into());
            }
        }
        validate_targets(&self.targets)?;
        validate_limits(
            &self.timeouts,
            self.forward_buffer_bytes,
            self.tunnel_buffer_bytes,
        )?;
        require(
            !self.app_known_hosts.as_os_str().is_empty(),
            "app known_hosts path is empty",
        )?;
        if let Some(proxy) = &self.proxy {
            proxy.validate()?;
        }
        Ok(())
    }
}

/// One SSH server of the chain, borrowed from the spec.
pub(crate) struct Hop<'a> {
    pub(crate) host: &'a str,
    pub(crate) port: u16,
    pub(crate) user: &'a str,
    pub(crate) auth: &'a Auth,
    /// `None` when there are no jumps (errors then stay exactly as they were before jump hosts existed).
    pub(crate) id: Option<HopId>,
}

impl Hop<'_> {
    fn info(&self, id: HopId) -> HopInfo {
        HopInfo {
            id,
            host: self.host.to_owned(),
            port: self.port,
            user: self.user.to_owned(),
        }
    }

    /// `jump 1 of 2 user@host:port` / `bastion user@host:port`; `None` without jumps.
    #[cfg(test)]
    pub(crate) fn label(&self) -> Option<String> {
        self.id.map(|id| self.info(id).to_string())
    }

    fn name(&self) -> String {
        self.info(self.id.unwrap_or(HopId::Bastion)).to_string()
    }

    fn validate(&self) -> Result<(), ConfigError> {
        let what = |text: &str| match self.id {
            Some(id) => format!("{}: {text}", self.info(id)),
            None => text.to_owned(),
        };
        validate_host(&what("SSH host"), self.host, false)?;
        require(self.port != 0, &what("SSH port must not be 0"))?;
        require(
            !self.user.is_empty() && !self.user.chars().any(char::is_control),
            &what("SSH user must be non-empty and free of control characters"),
        )?;
        match self.auth {
            Auth::KeyFile { path, .. } => require(
                !path.as_os_str().is_empty(),
                &what("private key path is empty"),
            ),
            Auth::Password(password) => {
                require(!password.is_empty(), &what("SSH password is empty"))
            }
            Auth::Agent { .. } | Auth::KeyboardInteractive => Ok(()),
        }
    }

    /// Adds the hop to an error, unless the error already says which host it is about.
    pub(crate) fn wrap(&self, error: TunnelError) -> TunnelError {
        match (self.id, error) {
            (
                Some(id),
                error @ (TunnelError::Connect { .. }
                | TunnelError::Auth(_)
                | TunnelError::ForwardRefused { .. }
                | TunnelError::Timeout { .. }
                | TunnelError::Closed),
            ) => TunnelError::Hop {
                hop: self.info(id),
                source: Box::new(error),
            },
            (_, error) => error,
        }
    }
}

pub(crate) fn validate_targets(targets: &[Target]) -> Result<(), ConfigError> {
    require(
        !targets.is_empty(),
        "at least one forward target is required",
    )?;
    for target in targets {
        validate_host("forward target host", &target.host, true)?;
        require(target.port != 0, "forward target port must not be 0")?;
    }
    Ok(())
}

pub(crate) fn validate_limits(
    t: &Timeouts,
    forward_buffer_bytes: usize,
    tunnel_buffer_bytes: usize,
) -> Result<(), ConfigError> {
    require(
        [
            t.connect,
            t.auth,
            t.forward,
            t.prompt,
            t.keepalive,
            t.forward_stall,
            t.outbound_liveness,
        ]
        .iter()
        .all(|d| !d.is_zero()),
        "timeouts must be greater than zero",
    )?;
    require(
        forward_buffer_bytes >= 64 << 10 && tunnel_buffer_bytes >= forward_buffer_bytes,
        "forward_buffer_bytes must be at least 64 KiB and not exceed tunnel_buffer_bytes",
    )
}

fn require(condition: bool, message: &str) -> Result<(), ConfigError> {
    condition
        .then_some(())
        .ok_or_else(|| ConfigError::Invalid(message.to_owned()))
}

pub(crate) fn validate_host(
    what: &str,
    host: &str,
    allow_underscore: bool,
) -> Result<(), ConfigError> {
    hostname::parse_host(host, allow_underscore)
        .map(|_| ())
        .map_err(|why| ConfigError::Invalid(format!("{what}: {why}")))
}

/// Set when the open that asked a question was abandoned (future dropped, prompt timed out). A [`Prompts`]
/// implementation that blocks on a human should watch it and give up.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    pub(crate) fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
}

/// One question of a keyboard-interactive round.
#[derive(Debug, Clone)]
pub struct PromptItem {
    pub text: String,
    /// Whether the answer may be echoed while typing (false for passwords and OTP codes).
    pub echo: bool,
}

#[derive(Debug, Clone)]
pub struct KeyboardInteractiveRequest {
    pub host: String,
    pub port: u16,
    pub name: String,
    pub instructions: String,
    pub prompts: Vec<PromptItem>,
    /// Becomes cancelled when this question no longer matters.
    pub cancel: CancelToken,
}

/// Callback used when the server needs input from a human. Called on a blocking worker thread (never on the
/// tunnel runtime), so it may block until the user answers; the wait is bounded by `Timeouts::prompt`.
pub trait Prompts: Send + Sync {
    /// One answer per prompt, in order, or `None` to cancel authentication.
    fn keyboard_interactive(&self, request: &KeyboardInteractiveRequest) -> Option<Vec<Secret>>;
}

/// Never answers: keyboard-interactive challenges fail with `AuthError::PromptDeclined`.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoPrompts;

impl Prompts for NoPrompts {
    fn keyboard_interactive(&self, _request: &KeyboardInteractiveRequest) -> Option<Vec<Secret>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> TunnelSpec {
        let mut spec = TunnelSpec::new(
            "bastion.example.com",
            "deploy",
            Auth::Password(Secret::new("pw-needle-123")),
            PathBuf::from("/tmp/app_known_hosts"),
        );
        spec.targets
            .push(Target::new("db.internal.example.com", 5432));
        spec
    }

    fn invalid(spec: &TunnelSpec) -> String {
        match spec.validate() {
            Err(TunnelError::Config(ConfigError::Invalid(message))) => message,
            other => panic!("expected an invalid-config error, got {other:?}"),
        }
    }

    #[test]
    fn a_complete_spec_validates() {
        spec().validate().unwrap();
    }

    #[test]
    fn rejects_bad_hosts_ports_and_users() {
        let mut s = spec();
        s.host = "-oProxyCommand=x".into();
        assert!(invalid(&s).contains("SSH host"));
        let mut s = spec();
        s.host = "a b".into();
        assert!(invalid(&s).contains("SSH host"));
        for bad in [
            "*,x.attacker.tld",
            "*",
            "a,b",
            "[::1]",
            "a|b",
            "a#b",
            "a@b",
            "a?b",
            "a!b",
            " a",
            "a\t",
        ] {
            let mut s = spec();
            s.host = bad.into();
            assert!(invalid(&s).contains("SSH host"), "{bad:?}");
        }
        let mut s = spec();
        s.port = 0;
        assert!(invalid(&s).contains("port"));
        let mut s = spec();
        s.user = String::new();
        assert!(invalid(&s).contains("user"));
        let mut s = spec();
        s.targets.clear();
        assert!(invalid(&s).contains("target"));
        let mut s = spec();
        s.targets[0].port = 0;
        assert!(invalid(&s).contains("port"));
        let mut s = spec();
        s.targets[0].host = "x\ny".into();
        assert!(invalid(&s).contains("target host"));
    }

    #[test]
    fn rejects_empty_credentials_and_zero_timeouts() {
        let mut s = spec();
        s.auth = Auth::Password(Secret::new(""));
        assert!(invalid(&s).contains("password"));
        let mut s = spec();
        s.auth = Auth::KeyFile {
            path: PathBuf::new(),
            passphrase: None,
        };
        assert!(invalid(&s).contains("key path"));
        let mut s = spec();
        s.timeouts.connect = Duration::ZERO;
        assert!(invalid(&s).contains("timeouts"));
    }

    #[test]
    fn buffer_budgets_are_validated() {
        let mut s = spec();
        s.forward_buffer_bytes = 1024;
        assert!(invalid(&s).contains("forward_buffer_bytes"));
        let mut s = spec();
        s.tunnel_buffer_bytes = s.forward_buffer_bytes - 1;
        assert!(invalid(&s).contains("forward_buffer_bytes"));
        let mut s = spec();
        s.timeouts.forward_stall = Duration::ZERO;
        assert!(invalid(&s).contains("timeouts"));
    }

    fn with_jumps(count: usize) -> TunnelSpec {
        let mut s = spec();
        s.jumps = (0..count)
            .map(|i| {
                Jump::new(
                    format!("jump{i}.example.com"),
                    "hopper",
                    Auth::Password(Secret::new("jump-pw-needle")),
                )
            })
            .collect();
        s
    }

    #[test]
    fn a_chain_of_valid_hops_validates_and_names_its_hops() {
        let s = with_jumps(2);
        s.validate().unwrap();
        let labels: Vec<_> = s.hops().into_iter().map(|h| h.label().unwrap()).collect();
        assert_eq!(
            labels,
            [
                "jump 1 of 2 hopper@jump0.example.com:22",
                "jump 2 of 2 hopper@jump1.example.com:22",
                "bastion deploy@bastion.example.com:22"
            ]
        );
        assert!(spec().hops()[0].id.is_none(), "no jumps: no hop context");
    }

    #[test]
    fn every_hop_is_validated_like_the_bastion() {
        let mut s = with_jumps(2);
        s.jumps[1].host = "bad host".into();
        let message = invalid(&s);
        assert!(
            message.contains("jump 2 of 2") && message.contains("SSH host"),
            "{message}"
        );
        let mut s = with_jumps(2);
        s.jumps[0].host = String::new();
        assert!(invalid(&s).contains("jump 1 of 2"));
        let mut s = with_jumps(1);
        s.jumps[0].port = 0;
        assert!(invalid(&s).contains("port must not be 0"));
        let mut s = with_jumps(1);
        s.jumps[0].user = String::new();
        assert!(invalid(&s).contains("user"));
        let mut s = with_jumps(1);
        s.jumps[0].auth = Auth::Password(Secret::new(""));
        assert!(invalid(&s).contains("password"));
        let mut s = with_jumps(1);
        s.jumps[0].auth = Auth::KeyFile {
            path: PathBuf::new(),
            passphrase: None,
        };
        assert!(invalid(&s).contains("key path"));
        // The secret of a hop never appears in a validation message.
        let mut s = with_jumps(1);
        s.jumps[0].user = "bad
user"
            .into();
        assert!(!invalid(&s).contains("jump-pw-needle"));
    }

    #[test]
    fn at_most_eight_jumps() {
        with_jumps(MAX_JUMPS).validate().unwrap();
        assert!(invalid(&with_jumps(MAX_JUMPS + 1)).contains("at most 8"));
    }

    #[test]
    fn a_repeated_host_and_port_in_the_chain_is_refused() {
        let mut s = with_jumps(2);
        s.jumps[1].host = "JUMP0.example.com".into();
        let message = invalid(&s);
        assert!(
            message.contains("repeats") && message.contains("jump 2 of 2"),
            "{message}"
        );
        // The bastion may not be one of its own jumps either...
        let mut s = with_jumps(1);
        s.jumps[0].host = s.host.clone();
        assert!(invalid(&s).contains("repeats"));
        // ...but the same host on another port is a different server.
        s.jumps[0].port = 2222;
        s.validate().unwrap();
    }

    #[test]
    fn the_proxy_is_validated() {
        use crate::proxy::{ProxyKind, ProxySpec};
        let mut s = spec();
        s.proxy = Some(ProxySpec::new(ProxyKind::Socks5, "proxy.example.com", 1080));
        s.validate().unwrap();
        s.proxy = Some(ProxySpec::new(ProxyKind::Socks5, "proxy.example.com", 0));
        assert!(invalid(&s).contains("proxy port"));
    }

    #[test]
    fn hop_errors_name_the_hop_but_leave_host_key_errors_alone() {
        let s = with_jumps(1);
        let hops = s.hops();
        let auth = hops[0].wrap(crate::error::AuthError::NoAgentIdentity.into());
        assert!(
            auth.to_string()
                .starts_with("jump 1 of 1 hopper@jump0.example.com:22: "),
            "{auth}"
        );
        assert_eq!(auth.code(), "tunnel.auth");
        assert!(matches!(auth.root(), TunnelError::Auth(_)));
        assert_eq!(
            auth.hop(),
            Some(&HopInfo {
                id: HopId::Jump { index: 0, of: 1 },
                host: "jump0.example.com".into(),
                port: 22,
                user: "hopper".into(),
            })
        );
        let bastion = s.hops()[1].wrap(TunnelError::Closed);
        assert_eq!(bastion.hop().map(|h| h.id), Some(HopId::Bastion));
        assert!(TunnelError::Closed.hop().is_none());
        let unknown = TunnelError::HostKeyUnknown {
            host: "jump0.example.com".into(),
            port: 22,
            algorithm: "a".into(),
            sha256_fingerprint: "f".into(),
            public_key: "k".into(),
        };
        assert!(matches!(
            hops[0].wrap(unknown),
            TunnelError::HostKeyUnknown { .. }
        ));
        // Without jumps nothing is wrapped (the error text is what it always was).
        assert!(matches!(
            spec().hops()[0].wrap(TunnelError::Closed),
            TunnelError::Closed
        ));
    }

    #[test]
    fn debug_never_shows_secrets() {
        let mut s = spec();
        s.auth = Auth::KeyFile {
            path: "/k".into(),
            passphrase: Some(Secret::new("pass-needle-456")),
        };
        let shown = format!("{s:?} {s:#?}");
        assert!(!shown.contains("pass-needle-456"));
        let shown = format!("{:?}", spec());
        assert!(!shown.contains("pw-needle-123"));
    }
}
