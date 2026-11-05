//! Which `host:port` of a connection URI an SSH tunnel has to forward, and the
//! URI the driver connects with once the tunnel's local ports are known.
//!
//! Pure string work (no GPUI, no engine-net): the rewrite rules per driver are
//! the "Driver TLS / endpoint override" table of `docs/plans/2026-10-11-ssh-tunnel.md`.
//!
//! * **PostgreSQL** keeps `host` and adds `hostaddr=127.0.0.1` plus the local port, so the
//!   socket goes to the tunnel while SNI and certificate verification use the real name.
//! * **MySQL / MariaDB** connect to `127.0.0.1:<local>` and add `tls-server-name=<real host>`
//!   whenever TLS may be used (`ssl-mode` is not `DISABLED`; `PREFERRED` is the default).
//! * **Redis / ClickHouse** have no server-name override yet: plain connections are rewritten,
//!   TLS is refused.
//! * **MongoDB** connects to the one forwarded member with `directConnection=true`; TLS,
//!   `mongodb+srv` and seed lists are refused.
//!
//! The URI is edited as text, never re-serialised, so percent-encoding of the user info, the
//! path and every parameter the plan does not touch survives byte for byte.

use std::net::SocketAddr;

use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};

use crate::records::ConnectionKind;

/// Everything that is not an RFC 3986 unreserved character.
const QUERY_VALUE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// A `host:port` as the bastion has to reach it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
}

impl Endpoint {
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
        }
    }
}

/// Why a connection cannot be opened through an SSH tunnel. `Display` is the text shown to the
/// user (and logged), so it names the way out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// SQLite (or any kind without a network endpoint).
    NoNetworkEndpoint(ConnectionKind),
    /// The URI cannot be split into host and port.
    InvalidUri(&'static str),
    /// The URI names no host (a local socket by default).
    NoHost,
    /// A Unix socket path instead of a network host.
    UnixSocket,
    /// More than one host in a kind that cannot forward several.
    MultipleHosts(ConnectionKind),
    /// Redis / ClickHouse: no way to keep verifying the real name yet.
    TlsUnsupported(ConnectionKind),
    MongoSrv,
    MongoTls,
    /// The URI is ambiguous (the host depends on which parser reads it) or has characters no
    /// driver URI needs: refused rather than guessed at.
    UnsafeUri(&'static str),
    /// The rewritten URI does not read back as the local forward: never connect with it.
    RewriteMismatch,
}

fn kind_name(kind: ConnectionKind) -> &'static str {
    match kind {
        ConnectionKind::Postgres => "PostgreSQL",
        ConnectionKind::Sqlite => "SQLite",
        ConnectionKind::Mongodb => "MongoDB",
        ConnectionKind::Mysql => "MySQL",
        ConnectionKind::Mariadb => "MariaDB",
        ConnectionKind::Redis => "Redis",
        ConnectionKind::Clickhouse => "ClickHouse",
    }
}

/// What the connection goes through, for the words of a refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// An SSH tunnel (with or without jump hosts and a proxy in front).
    Ssh,
    /// A proxy alone.
    Proxy,
}

impl PlanError {
    /// The text for `route`: a proxy-only connection is told about its proxy, not about an SSH
    /// tunnel it does not have.
    pub fn explain(&self, route: Route) -> String {
        let proxy = route == Route::Proxy;
        match self {
            Self::NoNetworkEndpoint(kind) if proxy => format!(
                "{} has no network address to reach through a proxy: turn the proxy off for this connection",
                kind_name(*kind)
            ),
            Self::NoHost if proxy => "Cannot use a proxy for this connection: the URI names no host".to_owned(),
            Self::UnixSocket if proxy => {
                "Cannot use a proxy for a Unix socket connection: use the TCP host and port of the database".to_owned()
            }
            Self::MultipleHosts(kind) if proxy => format!(
                "Cannot use a proxy for {} with several hosts in the URI: use a single host",
                kind_name(*kind)
            ),
            Self::InvalidUri(why) | Self::UnsafeUri(why) if proxy => {
                format!("Cannot use a proxy for this connection URI: {why}")
            }
            Self::TlsUnsupported(kind) if proxy => format!(
                "TLS to {} through a proxy is not supported yet: the driver cannot verify the server certificate against the real host name once it connects to 127.0.0.1. Turning TLS off here is not equivalent: the proxy does not encrypt anything, so the whole way to the database would be unencrypted and unauthenticated. Connect without the proxy, or only turn TLS off if that network is one you trust",
                kind_name(*kind)
            ),
            Self::MongoSrv if proxy => "MongoDB mongodb+srv:// (SRV) URIs cannot be used through a proxy here: the members are discovered by DNS and are not reachable through one forwarded port. Use a single mongodb://host:port address".to_owned(),
            Self::MongoTls if proxy => "TLS to MongoDB through a proxy is not supported yet: the driver verifies the certificate against the address it connects to, which is 127.0.0.1 behind a proxy route. Turning TLS off here is not equivalent: the proxy does not encrypt anything, so the whole way to the database would be unencrypted and unauthenticated. Connect without the proxy, or only turn TLS off if that network is one you trust".to_owned(),
            Self::RewriteMismatch if proxy => "Refusing to connect: the proxied URI does not point at the local proxy route port, so the database would be dialled directly".to_owned(),
            _ => self.ssh_text(),
        }
    }

    fn ssh_text(&self) -> String {
        match self {
            Self::NoNetworkEndpoint(kind) => format!(
                "{} has no network address to tunnel: turn the SSH tunnel off for this connection",
                kind_name(*kind)
            ),
            Self::InvalidUri(why) | Self::UnsafeUri(why) => {
                format!("Cannot tunnel this connection URI: {why}")
            }
            Self::NoHost => "Cannot tunnel this connection: the URI names no host (enter the host the SSH server reaches the database at)".to_owned(),
            Self::UnixSocket => "Cannot tunnel a Unix socket connection: use the TCP host and port of the database".to_owned(),
            Self::MultipleHosts(kind) => format!(
                "Cannot tunnel {} with several hosts in the URI: use a single host",
                kind_name(*kind)
            ),
            Self::TlsUnsupported(kind) => format!(
                "TLS to {} through an SSH tunnel is not supported yet: the driver cannot verify the server certificate against the real host name once it connects to 127.0.0.1. Turning TLS off here is not equivalent: the SSH tunnel only protects the leg to the SSH server, so the leg from the SSH server to the database would be unencrypted and unauthenticated. Connect without the tunnel, or only turn TLS off if that network is one you trust",
                kind_name(*kind)
            ),
            Self::MongoSrv => "MongoDB mongodb+srv:// (SRV) URIs cannot be used through an SSH tunnel: the members are discovered by DNS and are not reachable through one forwarded port. Use a single mongodb://host:port address".to_owned(),
            Self::MongoTls => "TLS to MongoDB through an SSH tunnel is not supported yet: the driver verifies the certificate against the address it connects to, which is 127.0.0.1 behind a tunnel. Turning TLS off here is not equivalent: the SSH tunnel only protects the leg to the SSH server, so the leg from the SSH server to the database would be unencrypted and unauthenticated. Connect without the tunnel, or only turn TLS off if that network is one you trust".to_owned(),
            Self::RewriteMismatch => "Refusing to connect: the tunnelled URI does not point at the local tunnel port, so the database would be dialled directly".to_owned(),
        }
    }
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.ssh_text())
    }
}

impl std::error::Error for PlanError {}

/// What to forward for one connection and how to rewrite its URI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelPlan {
    kind: ConnectionKind,
    uri: String,
    targets: Vec<Endpoint>,
}

impl TunnelPlan {
    /// Checks that `uri` can go through a tunnel and finds the endpoint to forward to.
    /// `remote` replaces the URI's own endpoint as the target (the database is known to the
    /// bastion under another name or port); the URI host still names the server for TLS.
    pub fn new(
        kind: ConnectionKind,
        uri: &str,
        remote: Option<&Endpoint>,
    ) -> Result<Self, PlanError> {
        let parts = Parts::parse(uri)?;
        let target = match kind {
            ConnectionKind::Sqlite => return Err(PlanError::NoNetworkEndpoint(kind)),
            ConnectionKind::Postgres => postgres_target(&parts)?,
            ConnectionKind::Mysql | ConnectionKind::Mariadb => mysql_target(&parts, kind)?,
            ConnectionKind::Redis => redis_target(&parts)?,
            ConnectionKind::Clickhouse => clickhouse_target(&parts)?,
            ConnectionKind::Mongodb => mongodb_target(&parts)?,
        };
        Ok(Self {
            kind,
            uri: uri.to_owned(),
            targets: vec![remote.cloned().unwrap_or(target)],
        })
    }

    /// The endpoints to forward, in the order [`Self::rewrite`] expects their local addresses.
    pub fn targets(&self) -> &[Endpoint] {
        &self.targets
    }

    /// The URI the driver connects with: `local[i]` is where `targets()[i]` is reachable.
    pub fn rewrite(&self, local: &[SocketAddr]) -> Result<String, PlanError> {
        let [address] = local else {
            return Err(PlanError::InvalidUri(
                "one local address per forwarded endpoint",
            ));
        };
        let parts = Parts::parse(&self.uri)?;
        let rewritten = match self.kind {
            ConnectionKind::Postgres => rewrite_postgres(&parts, *address),
            ConnectionKind::Mysql | ConnectionKind::Mariadb => rewrite_mysql(&parts, *address),
            ConnectionKind::Redis | ConnectionKind::Clickhouse => parts
                .with_endpoint(&address.ip().to_string(), address.port())
                .build(),
            ConnectionKind::Mongodb => rewrite_mongodb(&parts, *address),
            ConnectionKind::Sqlite => return Err(PlanError::NoNetworkEndpoint(self.kind)),
        };
        verify_forward(self.kind, &rewritten, *address)?;
        Ok(rewritten)
    }
}

/// Reads `uri` back the way the drivers' parsers do (credentials end at the FIRST `@` of the whole
/// text, as in tokio-postgres) and requires the endpoint it dials to be the local forward.
fn verify_forward(kind: ConnectionKind, uri: &str, local: SocketAddr) -> Result<(), PlanError> {
    let mismatch = || PlanError::RewriteMismatch;
    let rest = uri
        .split_once("://")
        .map(|(_, rest)| rest)
        .ok_or_else(mismatch)?;
    let after = rest.find('@').map_or(rest, |at| &rest[at + 1..]);
    let end = after.find(['/', '?', '#']).unwrap_or(after.len());
    let (authority, tail) = after.split_at(end);
    let query = tail
        .split_once('?')
        .map(|(_, query)| query.split('#').next().unwrap_or_default())
        .unwrap_or_default();
    let param = |key: &str| {
        query
            .split('&')
            .filter(|raw| !raw.is_empty())
            .map(Pair::new)
            .rev()
            .find(|pair| pair.key == key)
            .map(|pair| pair.value)
    };
    let entry = (!authority.is_empty())
        .then(|| parse_host_port(authority).ok())
        .flatten();
    let local_ip = local.ip().to_string();
    let ok = match kind {
        ConnectionKind::Postgres => {
            let port = entry
                .as_ref()
                .and_then(|entry| entry.port)
                .or_else(|| param("port").and_then(|port| port.parse().ok()));
            port == Some(local.port()) && param("hostaddr").as_deref() == Some(local_ip.as_str())
        }
        _ => entry
            .as_ref()
            .is_some_and(|entry| entry.name == local_ip && entry.port == Some(local.port())),
    };
    ok.then_some(()).ok_or_else(mismatch)
}

// ---------------------------------------------------------------------------------------------
// URI as text
// ---------------------------------------------------------------------------------------------

/// `scheme://[userinfo@]hosts[/path][?query][#fragment]`, each piece borrowed from the URI.
#[derive(Debug, Clone)]
struct Parts<'a> {
    scheme: &'a str,
    userinfo: Option<&'a str>,
    hosts: &'a str,
    path: &'a str,
    query: Option<&'a str>,
    fragment: Option<&'a str>,
}

/// One entry of the host list.
#[derive(Debug, PartialEq, Eq)]
struct HostPort<'a> {
    /// As written (IPv6 literals keep their brackets).
    text: &'a str,
    /// Percent-decoded, brackets removed.
    name: String,
    port: Option<u16>,
}

impl<'a> Parts<'a> {
    fn parse(uri: &'a str) -> Result<Self, PlanError> {
        if uri
            .chars()
            .any(|c| c == '\\' || c.is_whitespace() || c.is_control())
        {
            return Err(PlanError::UnsafeUri(
                "it contains a backslash, a space or a control character",
            ));
        }
        let (scheme, rest) = uri
            .split_once("://")
            .ok_or(PlanError::InvalidUri("missing scheme://"))?;
        let (rest, fragment) = match rest.split_once('#') {
            Some((rest, fragment)) => (rest, Some(fragment)),
            None => (rest, None),
        };
        let (rest, query) = match rest.split_once('?') {
            Some((rest, query)) => (rest, Some(query)),
            None => (rest, None),
        };
        let (authority, path) = match rest.find('/') {
            Some(slash) => rest.split_at(slash),
            None => (rest, ""),
        };
        // Parsers disagree about an `@` anywhere but once in the authority (tokio-postgres cuts the
        // credentials at the first `@` of the whole text), so there is exactly one place for it.
        let unsafe_at = authority.matches('@').count() > 1
            || path.contains('@')
            || query.is_some_and(|query| query.contains('@'))
            || fragment.is_some_and(|fragment| fragment.contains('@'));
        if unsafe_at {
            return Err(PlanError::UnsafeUri(
                "an @ is only allowed once, between the user info and the host: write any other @ (in a user name, a password or a query value) as %40",
            ));
        }
        let (userinfo, hosts) = match authority.rfind('@') {
            Some(at) => (Some(&authority[..at]), &authority[at + 1..]),
            None => (None, authority),
        };
        Ok(Self {
            scheme,
            userinfo,
            hosts,
            path,
            query,
            fragment,
        })
    }

    /// The host list entries (empty entries are kept so `a,,b` is not silently repaired).
    fn host_entries(&self) -> Result<Vec<HostPort<'a>>, PlanError> {
        if self.hosts.is_empty() {
            return Ok(Vec::new());
        }
        self.hosts.split(',').map(parse_host_port).collect()
    }

    /// `Some(entry)` for exactly one host, `None` for no host, an error for several.
    fn single_host(&self, kind: ConnectionKind) -> Result<Option<HostPort<'a>>, PlanError> {
        let mut entries = self.host_entries()?;
        match entries.len() {
            0 => Ok(None),
            1 => Ok(entries.pop()),
            _ => Err(PlanError::MultipleHosts(kind)),
        }
    }

    /// Query pairs as `(decoded lower-case key, decoded value, raw pair)` in order.
    fn pairs(&self) -> Vec<Pair> {
        self.query
            .unwrap_or_default()
            .split('&')
            .filter(|raw| !raw.is_empty())
            .map(Pair::new)
            .collect()
    }

    /// The last value of `key` (case-insensitive), the way the drivers' parsers overwrite.
    fn param(&self, key: &str) -> Option<String> {
        self.pairs()
            .into_iter()
            .rev()
            .find(|pair| pair.key == key)
            .map(|pair| pair.value)
    }

    /// The URI as it is, ready to be edited.
    fn editable(&self) -> Rebuilt<'a> {
        Rebuilt {
            parts: self.clone(),
            hosts: None,
            query: self.query.map(str::to_owned),
        }
    }

    /// The same URI with the host list replaced by `host:port`.
    fn with_endpoint(&self, host: &str, port: u16) -> Rebuilt<'a> {
        let mut rebuilt = self.editable();
        rebuilt.hosts = Some(format_host_port(host, Some(port)));
        rebuilt
    }
}

#[derive(Debug, Clone)]
struct Pair {
    key: String,
    value: String,
}

impl Pair {
    fn new(raw: &str) -> Self {
        let (key, value) = raw.split_once('=').unwrap_or((raw, ""));
        Self {
            key: decode_component(key).to_ascii_lowercase(),
            value: decode_component(value),
        }
    }
}

/// A URI being edited: pieces that changed are owned, the rest still borrowed.
struct Rebuilt<'a> {
    parts: Parts<'a>,
    hosts: Option<String>,
    query: Option<String>,
}

impl<'a> Rebuilt<'a> {
    /// Drops every pair whose key is in `keys` (lower case).
    fn without(mut self, keys: &[&str]) -> Self {
        let kept: Vec<&str> = self
            .query
            .as_deref()
            .unwrap_or_default()
            .split('&')
            .filter(|raw| !raw.is_empty())
            .filter(|raw| !keys.contains(&Pair::new(raw).key.as_str()))
            .collect();
        self.query = (!kept.is_empty()).then(|| kept.join("&"));
        self
    }

    /// Appends `key=value` (already encoded).
    fn with(mut self, pair: &str) -> Self {
        self.query = Some(match self.query.take() {
            Some(query) if !query.is_empty() => format!("{query}&{pair}"),
            _ => pair.to_owned(),
        });
        self
    }

    fn build(self) -> String {
        let mut uri = format!("{}://", self.parts.scheme);
        if let Some(userinfo) = self.parts.userinfo {
            uri.push_str(userinfo);
            uri.push('@');
        }
        uri.push_str(self.hosts.as_deref().unwrap_or(self.parts.hosts));
        uri.push_str(self.parts.path);
        let query = self.query.as_deref().filter(|query| !query.is_empty());
        // MongoDB's URI grammar wants the `/` before the options even without a database.
        if query.is_some() && self.parts.path.is_empty() && self.parts.scheme.starts_with("mongodb")
        {
            uri.push('/');
        }
        if let Some(query) = query {
            uri.push('?');
            uri.push_str(query);
        }
        if let Some(fragment) = self.parts.fragment {
            uri.push('#');
            uri.push_str(fragment);
        }
        uri
    }
}

fn decode_component(text: &str) -> String {
    percent_decode_str(text).decode_utf8_lossy().into_owned()
}

fn parse_host_port(entry: &str) -> Result<HostPort<'_>, PlanError> {
    if entry.is_empty() {
        return Err(PlanError::InvalidUri("an empty host in the host list"));
    }
    let (text, port_text) = if let Some(rest) = entry.strip_prefix('[') {
        let close = rest
            .find(']')
            .ok_or(PlanError::InvalidUri("unterminated [IPv6] literal"))?;
        let after = &rest[close + 1..];
        let port = match after {
            "" => None,
            after => Some(
                after
                    .strip_prefix(':')
                    .ok_or(PlanError::InvalidUri("text after the [IPv6] literal"))?,
            ),
        };
        (&entry[..close + 2], port)
    } else {
        match entry.split_once(':') {
            Some((host, port)) if !port.contains(':') => (host, Some(port)),
            Some(_) => return Err(PlanError::InvalidUri("an IPv6 literal needs [brackets]")),
            None => (entry, None),
        }
    };
    let port = port_text
        .map(|port| {
            port.parse::<u16>()
                .ok()
                .filter(|port| *port != 0)
                .ok_or(PlanError::InvalidUri(
                    "the port is not a number from 1 to 65535",
                ))
        })
        .transpose()?;
    let name = match text.strip_prefix('[') {
        Some(inside) => inside.trim_end_matches(']').to_owned(),
        None => decode_component(text),
    };
    Ok(HostPort { text, name, port })
}

fn format_host_port(host: &str, port: Option<u16>) -> String {
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    match port {
        Some(port) => format!("{host}:{port}"),
        None => host,
    }
}

fn looks_like_unix_socket(name: &str) -> bool {
    name.starts_with('/') || name.starts_with('\\')
}

/// The host entry that must exist and be a network host.
fn network_host<'a>(entry: Option<HostPort<'a>>) -> Result<HostPort<'a>, PlanError> {
    let entry = entry
        .filter(|entry| !entry.name.is_empty())
        .ok_or(PlanError::NoHost)?;
    if looks_like_unix_socket(&entry.name) {
        return Err(PlanError::UnixSocket);
    }
    Ok(entry)
}

fn is_truthy(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "true" | "1" | "yes" | "on"
    )
}

// ---------------------------------------------------------------------------------------------
// PostgreSQL
// ---------------------------------------------------------------------------------------------

const POSTGRES_PORT: u16 = 5432;

/// The authority host, or (libpq `postgresql:///db?host=name`) the `host` parameter.
struct PostgresHost {
    /// The name TLS is checked against.
    name: String,
    in_authority: bool,
    port: Option<u16>,
}

fn postgres_host(parts: &Parts<'_>) -> Result<PostgresHost, PlanError> {
    // The driver rejects both of these later with a vaguer error: say so here.
    if parts.fragment.is_some() {
        return Err(PlanError::UnsafeUri(
            "a PostgreSQL URI takes no #fragment (remove everything from the #)",
        ));
    }
    if !parts.hosts.is_empty() && parts.param("host").is_some() {
        return Err(PlanError::UnsafeUri(
            "it names a host in the address and again in a host= parameter: use only one",
        ));
    }
    match parts.single_host(ConnectionKind::Postgres)? {
        Some(entry) if !entry.name.is_empty() => {
            if looks_like_unix_socket(&entry.name) {
                return Err(PlanError::UnixSocket);
            }
            Ok(PostgresHost {
                name: entry.name,
                in_authority: true,
                port: entry.port,
            })
        }
        _ => {
            let name = parts.param("host").unwrap_or_default();
            if name.contains(',') {
                return Err(PlanError::MultipleHosts(ConnectionKind::Postgres));
            }
            if looks_like_unix_socket(&name) {
                return Err(PlanError::UnixSocket);
            }
            if name.is_empty() {
                return Err(PlanError::NoHost);
            }
            Ok(PostgresHost {
                name,
                in_authority: false,
                port: None,
            })
        }
    }
}

fn postgres_target(parts: &Parts<'_>) -> Result<Endpoint, PlanError> {
    let host = postgres_host(parts)?;
    // An explicit `hostaddr` is the address the TCP connection really goes to; the bastion
    // has to reach that one, while `host` stays the TLS name.
    let connect_to = match parts.param("hostaddr") {
        Some(addr) if addr.contains(',') => {
            return Err(PlanError::MultipleHosts(ConnectionKind::Postgres))
        }
        Some(addr) if !addr.is_empty() => addr,
        _ => host.name.clone(),
    };
    let port =
        match host.port {
            Some(port) => port,
            None => match parts.param("port") {
                None => POSTGRES_PORT,
                Some(text) if text.contains(',') => {
                    return Err(PlanError::MultipleHosts(ConnectionKind::Postgres))
                }
                Some(text) => text.parse::<u16>().ok().filter(|port| *port != 0).ok_or(
                    PlanError::InvalidUri("the port is not a number from 1 to 65535"),
                )?,
            },
        };
    Ok(Endpoint::new(connect_to, port))
}

fn rewrite_postgres(parts: &Parts<'_>, local: SocketAddr) -> String {
    // The port lives in one place only: the authority when the host is there, else `port=`.
    let mut rebuilt = parts.editable().without(&["hostaddr", "port"]);
    match postgres_host(parts) {
        Ok(host) if host.in_authority => {
            // The host text stays exactly as written (case, brackets).
            let text = parts
                .single_host(ConnectionKind::Postgres)
                .ok()
                .flatten()
                .map(|entry| entry.text)
                .unwrap_or_default();
            rebuilt.hosts = Some(format!("{text}:{}", local.port()));
        }
        _ => rebuilt = rebuilt.with(&format!("port={}", local.port())),
    }
    rebuilt.with(&format!("hostaddr={}", local.ip())).build()
}

// ---------------------------------------------------------------------------------------------
// MySQL / MariaDB
// ---------------------------------------------------------------------------------------------

const MYSQL_PORT: u16 = 3306;

fn mysql_target(parts: &Parts<'_>, kind: ConnectionKind) -> Result<Endpoint, PlanError> {
    if parts
        .param("socket")
        .is_some_and(|socket| !socket.is_empty())
    {
        return Err(PlanError::UnixSocket);
    }
    let entry = network_host(parts.single_host(kind)?)?;
    Ok(Endpoint::new(entry.name, entry.port.unwrap_or(MYSQL_PORT)))
}

/// `ssl-mode` / `sslmode` is anything but disabled (`PREFERRED` is the default).
fn mysql_uses_tls(parts: &Parts<'_>) -> bool {
    let mode = parts
        .pairs()
        .into_iter()
        .rev()
        .find(|pair| pair.key == "ssl-mode" || pair.key == "sslmode")
        .map(|pair| pair.value.to_ascii_uppercase());
    !matches!(mode.as_deref(), Some("DISABLED" | "DISABLE"))
}

fn rewrite_mysql(parts: &Parts<'_>, local: SocketAddr) -> String {
    let rebuilt = parts.with_endpoint(&local.ip().to_string(), local.port());
    let real_host = parts
        .single_host(ConnectionKind::Mysql)
        .ok()
        .flatten()
        .map(|entry| entry.name)
        .unwrap_or_default();
    let named = parts
        .param("tls-server-name")
        .is_some_and(|name| !name.is_empty());
    if mysql_uses_tls(parts) && !named && !real_host.is_empty() {
        // An empty `tls-server-name=` is replaced, not left next to the real one.
        rebuilt
            .without(&["tls-server-name"])
            .with(&format!(
                "tls-server-name={}",
                utf8_percent_encode(&real_host, QUERY_VALUE)
            ))
            .build()
    } else {
        rebuilt.build()
    }
}

// ---------------------------------------------------------------------------------------------
// Redis
// ---------------------------------------------------------------------------------------------

const REDIS_PORT: u16 = 6379;

fn redis_target(parts: &Parts<'_>) -> Result<Endpoint, PlanError> {
    match parts.scheme.to_ascii_lowercase().as_str() {
        "redis" => {}
        "rediss" => return Err(PlanError::TlsUnsupported(ConnectionKind::Redis)),
        _ => return Err(PlanError::UnixSocket),
    }
    let entry = network_host(parts.single_host(ConnectionKind::Redis)?)?;
    Ok(Endpoint::new(entry.name, entry.port.unwrap_or(REDIS_PORT)))
}

// ---------------------------------------------------------------------------------------------
// ClickHouse
// ---------------------------------------------------------------------------------------------

const CLICKHOUSE_PORT: u16 = 8123;

fn clickhouse_target(parts: &Parts<'_>) -> Result<Endpoint, PlanError> {
    let mut secure = match parts.scheme.to_ascii_lowercase().as_str() {
        "clickhouse" | "http" => false,
        "https" => true,
        _ => {
            return Err(PlanError::InvalidUri(
                "the scheme is not clickhouse://, http:// or https://",
            ))
        }
    };
    // The driver lets the last of `secure` / `ssl` win.
    for pair in parts.pairs() {
        if pair.key == "secure" || pair.key == "ssl" {
            secure = is_truthy(&pair.value);
        }
    }
    if secure {
        return Err(PlanError::TlsUnsupported(ConnectionKind::Clickhouse));
    }
    let entry = network_host(parts.single_host(ConnectionKind::Clickhouse)?)?;
    // `http://host` means port 80 like any URL; `clickhouse://host` means ClickHouse's own.
    let default = if parts.scheme.eq_ignore_ascii_case("http") {
        80
    } else {
        CLICKHOUSE_PORT
    };
    Ok(Endpoint::new(entry.name, entry.port.unwrap_or(default)))
}

// ---------------------------------------------------------------------------------------------
// MongoDB
// ---------------------------------------------------------------------------------------------

const MONGODB_PORT: u16 = 27017;

/// TLS is on for `tls=true` / `ssl=true`, and any other `tls*` option implies it unless the
/// switch is explicitly off.
fn mongodb_uses_tls(parts: &Parts<'_>) -> bool {
    let pairs = parts.pairs();
    let switch = pairs
        .iter()
        .rev()
        .find(|pair| pair.key == "tls" || pair.key == "ssl")
        .map(|pair| is_truthy(&pair.value));
    match switch {
        Some(on) => on,
        None => pairs.iter().any(|pair| pair.key.starts_with("tls")),
    }
}

fn mongodb_target(parts: &Parts<'_>) -> Result<Endpoint, PlanError> {
    if parts.scheme.eq_ignore_ascii_case("mongodb+srv") {
        return Err(PlanError::MongoSrv);
    }
    let entry = network_host(parts.single_host(ConnectionKind::Mongodb)?)?;
    if mongodb_uses_tls(parts) {
        return Err(PlanError::MongoTls);
    }
    Ok(Endpoint::new(
        entry.name,
        entry.port.unwrap_or(MONGODB_PORT),
    ))
}

fn rewrite_mongodb(parts: &Parts<'_>, local: SocketAddr) -> String {
    parts
        .with_endpoint(&local.ip().to_string(), local.port())
        .without(&["directconnection"])
        .with("directConnection=true")
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    fn plan(kind: ConnectionKind, uri: &str) -> TunnelPlan {
        TunnelPlan::new(kind, uri, None).unwrap_or_else(|error| panic!("{uri}: {error}"))
    }

    fn target(kind: ConnectionKind, uri: &str) -> (String, u16) {
        let plan = plan(kind, uri);
        let [endpoint] = plan.targets() else {
            panic!("one target");
        };
        (endpoint.host.clone(), endpoint.port)
    }

    fn rewritten(kind: ConnectionKind, uri: &str) -> String {
        plan(kind, uri).rewrite(&[local(40001)]).unwrap()
    }

    fn refusal(kind: ConnectionKind, uri: &str) -> PlanError {
        TunnelPlan::new(kind, uri, None).expect_err(uri)
    }

    // --- PostgreSQL -------------------------------------------------------------------------

    #[test]
    fn postgres_keeps_the_host_and_adds_hostaddr_and_the_local_port() {
        assert_eq!(
            rewritten(
                ConnectionKind::Postgres,
                "postgres://u:p%40ss@db.example.com:6543/app?sslmode=verify-full"
            ),
            "postgres://u:p%40ss@db.example.com:40001/app?sslmode=verify-full&hostaddr=127.0.0.1"
        );
        assert_eq!(
            target(
                ConnectionKind::Postgres,
                "postgres://u@db.example.com:6543/app"
            ),
            ("db.example.com".into(), 6543)
        );
    }

    #[test]
    fn postgres_default_port_is_added_to_the_authority() {
        assert_eq!(
            target(ConnectionKind::Postgres, "postgresql://db.example.com/app"),
            ("db.example.com".into(), 5432)
        );
        assert_eq!(
            rewritten(ConnectionKind::Postgres, "postgresql://db.example.com/app"),
            "postgresql://db.example.com:40001/app?hostaddr=127.0.0.1"
        );
    }

    #[test]
    fn postgres_ipv6_host_keeps_its_brackets() {
        assert_eq!(
            target(
                ConnectionKind::Postgres,
                "postgres://u@[2001:db8::5]:5433/d"
            ),
            ("2001:db8::5".into(), 5433)
        );
        assert_eq!(
            rewritten(
                ConnectionKind::Postgres,
                "postgres://u@[2001:db8::5]:5433/d"
            ),
            "postgres://u@[2001:db8::5]:40001/d?hostaddr=127.0.0.1"
        );
        assert_eq!(
            rewritten(ConnectionKind::Postgres, "postgres://[::1]/d"),
            "postgres://[::1]:40001/d?hostaddr=127.0.0.1"
        );
    }

    #[test]
    fn postgres_existing_hostaddr_and_port_params_are_replaced_and_hostaddr_is_the_target() {
        // The explicit hostaddr is where the TCP connection really goes, so it is what the bastion reaches.
        assert_eq!(
            target(
                ConnectionKind::Postgres,
                "postgres://db.example.com:5432/d?hostaddr=192.0.2.7"
            ),
            ("192.0.2.7".into(), 5432)
        );
        assert_eq!(
            rewritten(ConnectionKind::Postgres, "postgres://db.example.com:5432/d?application_name=x&hostaddr=192.0.2.7&sslmode=require"),
            "postgres://db.example.com:40001/d?application_name=x&sslmode=require&hostaddr=127.0.0.1"
        );
    }

    #[test]
    fn postgres_host_in_the_query_gets_hostaddr_and_a_port_param() {
        assert_eq!(
            target(
                ConnectionKind::Postgres,
                "postgresql:///d?host=db.example.com&port=6000"
            ),
            ("db.example.com".into(), 6000)
        );
        assert_eq!(
            rewritten(
                ConnectionKind::Postgres,
                "postgresql:///d?host=db.example.com&port=6000&sslmode=verify-full"
            ),
            "postgresql:///d?host=db.example.com&sslmode=verify-full&port=40001&hostaddr=127.0.0.1"
        );
    }

    #[test]
    fn postgres_other_parameters_survive_byte_for_byte() {
        let uri = "postgres://u@h.example.com/d?sslrootcert=%2Ftmp%2Fca%20x.pem&options=-c%20search_path%3Da%2Bb&application_name=a+b";
        assert_eq!(
            rewritten(ConnectionKind::Postgres, uri),
            "postgres://u@h.example.com:40001/d?sslrootcert=%2Ftmp%2Fca%20x.pem&options=-c%20search_path%3Da%2Bb&application_name=a+b&hostaddr=127.0.0.1"
        );
    }

    #[test]
    fn postgres_refusals() {
        assert_eq!(
            refusal(ConnectionKind::Postgres, "postgres://a:5432,b:5432/d"),
            PlanError::MultipleHosts(ConnectionKind::Postgres)
        );
        assert_eq!(
            refusal(ConnectionKind::Postgres, "postgres:///d"),
            PlanError::NoHost
        );
        assert_eq!(
            refusal(
                ConnectionKind::Postgres,
                "postgresql:///d?host=/var/run/postgresql"
            ),
            PlanError::UnixSocket
        );
        assert_eq!(
            refusal(ConnectionKind::Postgres, "postgres://%2Fvar%2Frun/d"),
            PlanError::UnixSocket
        );
        assert_eq!(
            refusal(
                ConnectionKind::Postgres,
                "postgres://h/d?hostaddr=1.2.3.4,5.6.7.8"
            ),
            PlanError::MultipleHosts(ConnectionKind::Postgres)
        );
        assert!(matches!(
            refusal(ConnectionKind::Postgres, "postgres://h:0/d"),
            PlanError::InvalidUri(_)
        ));
        assert!(matches!(
            refusal(ConnectionKind::Postgres, "postgres://h:99999/d"),
            PlanError::InvalidUri(_)
        ));
    }

    // --- MySQL / MariaDB --------------------------------------------------------------------

    #[test]
    fn mysql_connects_to_loopback_and_keeps_the_tls_name() {
        assert_eq!(
            rewritten(ConnectionKind::Mysql, "mysql://root:s3%2Fcret@db.example.com:3307/app?ssl-mode=VERIFY_IDENTITY"),
            "mysql://root:s3%2Fcret@127.0.0.1:40001/app?ssl-mode=VERIFY_IDENTITY&tls-server-name=db.example.com"
        );
        assert_eq!(
            target(ConnectionKind::Mysql, "mysql://u@db.example.com:3307/app"),
            ("db.example.com".into(), 3307)
        );
    }

    #[test]
    fn mysql_default_ssl_mode_is_tls_first_so_the_name_is_added() {
        assert_eq!(
            rewritten(ConnectionKind::Mariadb, "mariadb://u@db.example.com/app"),
            "mariadb://u@127.0.0.1:40001/app?tls-server-name=db.example.com"
        );
        assert_eq!(
            target(ConnectionKind::Mariadb, "mariadb://u@db.example.com/app"),
            ("db.example.com".into(), 3306)
        );
    }

    #[test]
    fn mysql_disabled_tls_gets_no_name_in_either_spelling() {
        for spelling in [
            "ssl-mode=DISABLED",
            "ssl-mode=disabled",
            "sslmode=disable",
            "sslmode=DISABLED",
        ] {
            assert_eq!(
                rewritten(
                    ConnectionKind::Mysql,
                    &format!("mysql://u@db.example.com/d?{spelling}")
                ),
                format!("mysql://u@127.0.0.1:40001/d?{spelling}"),
                "{spelling}"
            );
        }
    }

    #[test]
    fn mysql_existing_tls_server_name_is_kept_and_not_duplicated() {
        assert_eq!(
            rewritten(
                ConnectionKind::Mysql,
                "mysql://u@10.0.0.9/d?ssl-mode=REQUIRED&tls-server-name=db.example.com"
            ),
            "mysql://u@127.0.0.1:40001/d?ssl-mode=REQUIRED&tls-server-name=db.example.com"
        );
    }

    #[test]
    fn mysql_ipv6_host_and_percent_encoded_name() {
        assert_eq!(
            target(ConnectionKind::Mysql, "mysql://u@[2001:db8::9]:3306/d"),
            ("2001:db8::9".into(), 3306)
        );
        assert_eq!(
            rewritten(ConnectionKind::Mysql, "mysql://u@[2001:db8::9]:3306/d"),
            "mysql://u@127.0.0.1:40001/d?tls-server-name=2001%3Adb8%3A%3A9"
        );
        assert_eq!(
            rewritten(ConnectionKind::Mysql, "mysql://u@d%62.example.com/d"),
            "mysql://u@127.0.0.1:40001/d?tls-server-name=db.example.com"
        );
    }

    #[test]
    fn mysql_refusals() {
        assert_eq!(
            refusal(ConnectionKind::Mysql, "mysql://a,b/d"),
            PlanError::MultipleHosts(ConnectionKind::Mysql)
        );
        assert_eq!(
            refusal(ConnectionKind::Mysql, "mysql:///d"),
            PlanError::NoHost
        );
        assert_eq!(
            refusal(
                ConnectionKind::Mysql,
                "mysql://localhost/d?socket=%2Ftmp%2Fmysql.sock"
            ),
            PlanError::UnixSocket
        );
    }

    // --- Redis ------------------------------------------------------------------------------

    #[test]
    fn redis_plain_is_rewritten_and_keeps_user_info_and_database() {
        assert_eq!(
            rewritten(
                ConnectionKind::Redis,
                "redis://default:p%3Aw@cache.example.com:6380/3"
            ),
            "redis://default:p%3Aw@127.0.0.1:40001/3"
        );
        assert_eq!(
            target(ConnectionKind::Redis, "redis://cache.example.com"),
            ("cache.example.com".into(), 6379)
        );
        assert_eq!(
            rewritten(ConnectionKind::Redis, "redis://[::1]/0"),
            "redis://127.0.0.1:40001/0"
        );
    }

    #[test]
    fn redis_tls_is_refused_with_the_documented_message() {
        let error = refusal(ConnectionKind::Redis, "rediss://cache.example.com:6380/0");
        assert_eq!(error, PlanError::TlsUnsupported(ConnectionKind::Redis));
        assert!(
            error
                .to_string()
                .starts_with("TLS to Redis through an SSH tunnel is not supported yet"),
            "{error}"
        );
        assert_eq!(
            refusal(ConnectionKind::Redis, "REDISS://x"),
            PlanError::TlsUnsupported(ConnectionKind::Redis)
        );
        assert_eq!(
            refusal(ConnectionKind::Redis, "redis+unix:///tmp/redis.sock"),
            PlanError::UnixSocket
        );
        assert_eq!(
            refusal(ConnectionKind::Redis, "redis:///0"),
            PlanError::NoHost
        );
        assert_eq!(
            refusal(ConnectionKind::Redis, "redis://a:1,b:2/0"),
            PlanError::MultipleHosts(ConnectionKind::Redis)
        );
    }

    // --- ClickHouse -------------------------------------------------------------------------

    #[test]
    fn clickhouse_plain_http_is_rewritten() {
        assert_eq!(
            rewritten(
                ConnectionKind::Clickhouse,
                "clickhouse://default:pw@ch.example.com:8123/db?max_execution_time=30"
            ),
            "clickhouse://default:pw@127.0.0.1:40001/db?max_execution_time=30"
        );
        assert_eq!(
            target(ConnectionKind::Clickhouse, "clickhouse://ch.example.com"),
            ("ch.example.com".into(), 8123)
        );
        assert_eq!(
            target(ConnectionKind::Clickhouse, "http://ch.example.com"),
            ("ch.example.com".into(), 80)
        );
        assert_eq!(
            rewritten(ConnectionKind::Clickhouse, "http://ch.example.com/db"),
            "http://127.0.0.1:40001/db"
        );
    }

    #[test]
    fn clickhouse_tls_in_every_spelling_is_refused() {
        for uri in [
            "https://ch.example.com/db",
            "clickhouse://ch.example.com:8443/db?secure=true",
            "clickhouse://ch.example.com/db?ssl=1",
            "clickhouse://ch.example.com/db?ssl=YES",
            "clickhouse://ch.example.com/db?secure=false&ssl=on",
        ] {
            assert_eq!(
                refusal(ConnectionKind::Clickhouse, uri),
                PlanError::TlsUnsupported(ConnectionKind::Clickhouse),
                "{uri}"
            );
        }
        // The last switch wins, as in the driver.
        assert_eq!(
            target(
                ConnectionKind::Clickhouse,
                "https://ch.example.com:8123/?secure=false"
            )
            .1,
            8123
        );
    }

    // --- MongoDB ----------------------------------------------------------------------------

    #[test]
    fn mongodb_single_host_forces_a_direct_connection() {
        assert_eq!(
            rewritten(ConnectionKind::Mongodb, "mongodb://root:p%40@mongo.example.com:27018/app?authSource=admin&replicaSet=rs0"),
            "mongodb://root:p%40@127.0.0.1:40001/app?authSource=admin&replicaSet=rs0&directConnection=true"
        );
        assert_eq!(
            target(ConnectionKind::Mongodb, "mongodb://mongo.example.com/app"),
            ("mongo.example.com".into(), 27017)
        );
    }

    #[test]
    fn mongodb_existing_direct_connection_is_replaced() {
        assert_eq!(
            rewritten(
                ConnectionKind::Mongodb,
                "mongodb://m.example.com/?directConnection=false&appName=x"
            ),
            "mongodb://127.0.0.1:40001/?appName=x&directConnection=true"
        );
    }

    #[test]
    fn mongodb_refusals() {
        assert_eq!(
            refusal(ConnectionKind::Mongodb, "mongodb+srv://c.example.com/db"),
            PlanError::MongoSrv
        );
        assert_eq!(
            refusal(
                ConnectionKind::Mongodb,
                "mongodb://a.example.com:27017,b.example.com:27017/db?replicaSet=rs"
            ),
            PlanError::MultipleHosts(ConnectionKind::Mongodb)
        );
        for uri in [
            "mongodb://m.example.com/db?tls=true",
            "mongodb://m.example.com/db?ssl=true",
            "mongodb://m.example.com/db?TLS=1",
            "mongodb://m.example.com/db?tlsCAFile=%2Ftmp%2Fca.pem",
            "mongodb://m.example.com/db?tlsAllowInvalidCertificates=true",
        ] {
            assert_eq!(
                refusal(ConnectionKind::Mongodb, uri),
                PlanError::MongoTls,
                "{uri}"
            );
        }
        // An explicit off switch wins over a stray option.
        assert!(TunnelPlan::new(
            ConnectionKind::Mongodb,
            "mongodb://m.example.com/db?tls=false",
            None
        )
        .is_ok());
        assert_eq!(
            refusal(
                ConnectionKind::Mongodb,
                "mongodb://%2Ftmp%2Fmongodb-27017.sock/db"
            ),
            PlanError::UnixSocket
        );
    }

    #[test]
    fn mongodb_ipv6_and_default_port() {
        assert_eq!(
            target(ConnectionKind::Mongodb, "mongodb://[2001:db8::1]/db"),
            ("2001:db8::1".into(), 27017)
        );
        assert_eq!(
            rewritten(ConnectionKind::Mongodb, "mongodb://[2001:db8::1]:27020/db"),
            "mongodb://127.0.0.1:40001/db?directConnection=true"
        );
    }

    // --- shared -----------------------------------------------------------------------------

    #[test]
    fn sqlite_has_nothing_to_tunnel() {
        assert_eq!(
            refusal(ConnectionKind::Sqlite, "sqlite:///tmp/x.db"),
            PlanError::NoNetworkEndpoint(ConnectionKind::Sqlite)
        );
    }

    #[test]
    fn a_remote_override_replaces_the_target_but_not_the_tls_name() {
        let remote = Endpoint::new("10.1.2.3", 15432);
        let plan = TunnelPlan::new(
            ConnectionKind::Postgres,
            "postgres://u@db.example.com:5432/d?sslmode=verify-full",
            Some(&remote),
        )
        .unwrap();
        assert_eq!(plan.targets(), [remote]);
        assert_eq!(
            plan.rewrite(&[local(40002)]).unwrap(),
            "postgres://u@db.example.com:40002/d?sslmode=verify-full&hostaddr=127.0.0.1"
        );
        let mysql = TunnelPlan::new(
            ConnectionKind::Mysql,
            "mysql://u@db.example.com/d",
            Some(&Endpoint::new("10.1.2.3", 3307)),
        )
        .unwrap();
        assert_eq!(
            mysql.rewrite(&[local(40003)]).unwrap(),
            "mysql://u@127.0.0.1:40003/d?tls-server-name=db.example.com"
        );
    }

    #[test]
    fn fragments_userinfo_and_a_missing_path_survive() {
        assert_eq!(
            rewritten(ConnectionKind::Redis, "redis://u:p@h.example.com#frag"),
            "redis://u:p@127.0.0.1:40001#frag"
        );
        assert_eq!(
            rewritten(
                ConnectionKind::Postgres,
                "postgres://u:a%40b@h.example.com/d"
            ),
            "postgres://u:a%40b@h.example.com:40001/d?hostaddr=127.0.0.1"
        );
    }

    #[test]
    fn an_unencoded_at_sign_in_a_query_value_is_refused_with_the_percent_40_hint() {
        for (kind, uri) in [
            (
                ConnectionKind::Postgres,
                "postgres://u@h.example.com/d?options=a@b",
            ),
            (
                ConnectionKind::Mysql,
                "mysql://u@h.example.com/d?init-command=x@y",
            ),
            (
                ConnectionKind::Mongodb,
                "mongodb://h.example.com/?appName=me@corp",
            ),
        ] {
            let error = refusal(kind, uri);
            assert!(matches!(error, PlanError::UnsafeUri(_)), "{uri}");
            assert!(
                error.to_string().contains("%40") && error.to_string().contains("query value"),
                "{error}"
            );
        }
        // Written as %40 it passes through untouched.
        assert_eq!(
            rewritten(
                ConnectionKind::Postgres,
                "postgres://u@h.example.com/d?options=a%40b"
            ),
            "postgres://u@h.example.com:40001/d?options=a%40b&hostaddr=127.0.0.1"
        );
    }

    #[test]
    fn postgres_fragment_and_a_second_host_are_refused_with_a_clear_message() {
        let fragment = refusal(
            ConnectionKind::Postgres,
            "postgres://u@h.example.com/d#frag",
        );
        assert!(matches!(fragment, PlanError::UnsafeUri(_)));
        assert!(fragment.to_string().contains("#fragment"), "{fragment}");
        let both = refusal(
            ConnectionKind::Postgres,
            "postgres://u@h.example.com/d?host=other.example.com",
        );
        assert!(matches!(both, PlanError::UnsafeUri(_)));
        assert!(both.to_string().contains("host= parameter"), "{both}");
        // Only the host in the query (no authority host) is fine, and other kinds keep fragments.
        assert!(TunnelPlan::new(
            ConnectionKind::Postgres,
            "postgresql:///d?host=h.example.com",
            None
        )
        .is_ok());
        assert!(
            TunnelPlan::new(ConnectionKind::Redis, "redis://h.example.com/0#frag", None).is_ok()
        );
    }

    #[test]
    fn an_at_sign_outside_the_authority_is_refused() {
        // tokio-postgres would read the host after the FIRST `@` of the whole text.
        for (kind, uri) in [
            (
                ConnectionKind::Postgres,
                "postgres://x:1?dbname=y#z@db.internal/app",
            ),
            (
                ConnectionKind::Postgres,
                "postgres://x:1/app?options=a@db.internal",
            ),
            (ConnectionKind::Postgres, "postgres://u:a@b@h.example.com/d"),
            (ConnectionKind::Postgres, "postgres://u@h.example.com/d@x"),
            (ConnectionKind::Mysql, "mysql://u:p@w@db.example.com/d"),
            (ConnectionKind::Redis, "redis://h.example.com/0#a@b"),
            (
                ConnectionKind::Mongodb,
                "mongodb://h.example.com/?appName=a@b",
            ),
            (
                ConnectionKind::Clickhouse,
                "clickhouse://u@h.example.com/d?x=1@2",
            ),
        ] {
            assert!(
                matches!(refusal(kind, uri), PlanError::UnsafeUri(_)),
                "{uri}"
            );
        }
        for uri in [
            "postgres://u@h.example.com/d\\x",
            "postgres://u@h.example.com/d x",
            "postgres://u@h.example.com/d\t",
            "postgres://u@h.example.com/d\u{7}",
            "postgres://u@h.example.com/ d",
        ] {
            assert!(
                matches!(
                    refusal(ConnectionKind::Postgres, uri),
                    PlanError::UnsafeUri(_)
                ),
                "{uri:?}"
            );
        }
        // An encoded `@` is fine.
        assert!(TunnelPlan::new(
            ConnectionKind::Postgres,
            "postgres://u%40corp:p%40ss@h.example.com/d",
            None
        )
        .is_ok());
    }

    /// Every URI a connection of each kind can have: what the form writes, with TLS options, IPv6,
    /// encoded credentials, query-only hosts, no path.
    const CORPUS: &[(ConnectionKind, &str)] = &[
        (
            ConnectionKind::Postgres,
            "postgres://u:p%40ss@db.example.com:6543/app?sslmode=verify-full",
        ),
        (ConnectionKind::Postgres, "postgresql://db.example.com/app"),
        (
            ConnectionKind::Postgres,
            "postgres://u@[2001:db8::5]:5433/d",
        ),
        (
            ConnectionKind::Postgres,
            "postgres://db.example.com:5432/d?hostaddr=192.0.2.7&application_name=x",
        ),
        (
            ConnectionKind::Postgres,
            "postgresql:///d?host=db.example.com&port=6000&sslmode=require",
        ),
        (ConnectionKind::Postgres, "postgres://u@h.example.com"),
        (
            ConnectionKind::Mysql,
            "mysql://root:s3%2Fcret@db.example.com:3307/app?ssl-mode=VERIFY_IDENTITY",
        ),
        (
            ConnectionKind::Mysql,
            "mysql://u@db.example.com/app?ssl-mode=DISABLED",
        ),
        (
            ConnectionKind::Mysql,
            "mysql://u@[2001:db8::9]:3306/d?tls-server-name=",
        ),
        (ConnectionKind::Mariadb, "mariadb://u@db.example.com"),
        (
            ConnectionKind::Redis,
            "redis://default:p%3Aw@cache.example.com:6380/3",
        ),
        (ConnectionKind::Redis, "redis://cache.example.com"),
        (ConnectionKind::Redis, "redis://u:p@h.example.com#frag"),
        (
            ConnectionKind::Clickhouse,
            "clickhouse://default:pw@ch.example.com:8123/db?max_execution_time=30",
        ),
        (ConnectionKind::Clickhouse, "http://ch.example.com/db"),
        (
            ConnectionKind::Mongodb,
            "mongodb://root:p%40@mongo.example.com:27018/app?authSource=admin&replicaSet=rs0",
        ),
        (ConnectionKind::Mongodb, "mongodb://mongo.example.com"),
        (
            ConnectionKind::Mongodb,
            "mongodb://[2001:db8::1]/db?directConnection=false",
        ),
    ];

    #[test]
    fn rewritten_postgres_uri_parses_to_the_local_endpoint() {
        // The same property for every kind: read back with the drivers' first-`@` rule, the URI
        // dials the local forward and nothing else.
        for (kind, uri) in CORPUS {
            let plan = plan(*kind, uri);
            let rewritten = plan
                .rewrite(&[local(40007)])
                .unwrap_or_else(|error| panic!("{uri}: {error}"));
            verify_forward(*kind, &rewritten, local(40007))
                .unwrap_or_else(|error| panic!("{rewritten}: {error}"));
            assert!(
                !rewritten.contains("example.com:") || *kind == ConnectionKind::Postgres,
                "{rewritten}"
            );
            // A different local port is a mismatch: the check is not vacuous.
            assert_eq!(
                verify_forward(*kind, &rewritten, local(40008)),
                Err(PlanError::RewriteMismatch),
                "{rewritten}"
            );
        }
        // And a URI that still names the real host is refused.
        assert_eq!(
            verify_forward(
                ConnectionKind::Postgres,
                "postgres://u@db.internal:5432/app",
                local(40007)
            ),
            Err(PlanError::RewriteMismatch)
        );
        assert_eq!(
            verify_forward(
                ConnectionKind::Redis,
                "redis://x:1@db.internal:6379",
                local(40007)
            ),
            Err(PlanError::RewriteMismatch)
        );
    }

    #[test]
    fn mysql_empty_tls_server_name_is_replaced_not_duplicated() {
        assert_eq!(
            rewritten(
                ConnectionKind::Mysql,
                "mysql://u@db.example.com/d?tls-server-name=&ssl-mode=REQUIRED"
            ),
            "mysql://u@127.0.0.1:40001/d?ssl-mode=REQUIRED&tls-server-name=db.example.com"
        );
    }

    #[test]
    fn mongodb_uri_without_a_path_rewrites_to_a_form_the_driver_accepts() {
        // `mongodb://host?opt=1` is not valid MongoDB URI syntax: the `/` must precede the options.
        assert_eq!(
            rewritten(ConnectionKind::Mongodb, "mongodb://mongo.example.com"),
            "mongodb://127.0.0.1:40001/?directConnection=true"
        );
        assert_eq!(
            rewritten(
                ConnectionKind::Mongodb,
                "mongodb://u:p@mongo.example.com:27018?appName=x"
            ),
            "mongodb://u:p@127.0.0.1:40001/?appName=x&directConnection=true"
        );
        // Other schemes keep what they had.
        assert_eq!(
            rewritten(
                ConnectionKind::Redis,
                "redis://u:p@h.example.com?protocol=resp3"
            ),
            "redis://u:p@127.0.0.1:40001?protocol=resp3"
        );
    }

    #[test]
    fn one_local_address_per_target_is_required() {
        let plan = plan(ConnectionKind::Redis, "redis://h.example.com");
        assert!(matches!(plan.rewrite(&[]), Err(PlanError::InvalidUri(_))));
        assert!(matches!(
            plan.rewrite(&[local(1), local(2)]),
            Err(PlanError::InvalidUri(_))
        ));
    }

    #[test]
    fn malformed_uris_are_typed_errors_not_panics() {
        for uri in [
            "",
            "no-scheme",
            "postgres://[::1/d",
            "postgres://[::1]x/d",
            "postgres://a::b/d",
        ] {
            assert!(
                matches!(
                    TunnelPlan::new(ConnectionKind::Postgres, uri, None),
                    Err(PlanError::InvalidUri(_))
                ),
                "{uri}"
            );
        }
    }

    #[test]
    fn refusal_texts_name_the_way_out_and_never_a_secret() {
        for error in [
            PlanError::TlsUnsupported(ConnectionKind::Clickhouse),
            PlanError::MongoTls,
            PlanError::MongoSrv,
            PlanError::MultipleHosts(ConnectionKind::Mongodb),
            PlanError::UnixSocket,
            PlanError::NoHost,
        ] {
            let text = error.to_string();
            assert!(text.len() > 40, "{text}");
        }
        assert!(PlanError::TlsUnsupported(ConnectionKind::Clickhouse)
            .to_string()
            .starts_with("TLS to ClickHouse through an SSH tunnel is not supported yet"));
        assert!(PlanError::MongoTls
            .to_string()
            .starts_with("TLS to MongoDB through an SSH tunnel is not supported yet"));
        // Turning TLS off is not presented as the safe way out.
        for error in [
            PlanError::MongoTls,
            PlanError::TlsUnsupported(ConnectionKind::Redis),
        ] {
            let text = error.to_string();
            assert!(text.contains("unencrypted and unauthenticated"), "{text}");
            assert!(!text.contains("already encrypts"), "{text}");
        }
    }
}
