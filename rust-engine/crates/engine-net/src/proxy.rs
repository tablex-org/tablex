//! SOCKS5 and HTTP CONNECT proxy clients, and the proxy-only route ([`ProxyRoute`], `Tunnel::open_proxy`).
//!
//! Written from the protocol specifications: RFC 1928 (SOCKS5), RFC 1929 (username/password sub-negotiation
//! for SOCKS5), RFC 9110 section 9.3.6 (the CONNECT method) and RFC 7617 (the Basic scheme). The exchanges are
//! generic over any byte stream, so they are tested against in-memory pipes.
//!
//! The target is handed to the proxy as a **name** whenever it is one (the proxy resolves it, so the client
//! needs no DNS access to the target network); only an IP literal is sent as an address. Replies are read to
//! their exact end, so bytes the proxy forwards from the target right after its answer stay in the socket for
//! the tunnelled protocol.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use zeroize::Zeroizing;

use crate::error::{ConfigError, TunnelError};
use crate::secret::Secret;
use crate::spec::{
    validate_host, validate_limits, validate_targets, Target, Timeouts,
    DEFAULT_FORWARD_BUFFER_BYTES, DEFAULT_MAX_CONCURRENT_FORWARDS, DEFAULT_TUNNEL_BUFFER_BYTES,
};

/// Longest response head (status line and headers) accepted from an HTTP proxy.
const HEAD_LIMIT: usize = 8 * 1024;
/// How many `1xx` (informational) heads are skipped in front of the final status.
const INFORMATIONAL_LIMIT: usize = 4;
/// Longest piece of a proxy status line quoted in an error.
const QUOTE_LIMIT: usize = 120;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyKind {
    Socks5,
    HttpConnect,
}

/// Proxy login: SOCKS5 username/password (RFC 1929) or `Proxy-Authorization: Basic` (RFC 7617).
#[derive(Debug, Clone)]
pub struct ProxyCredentials {
    pub user: String,
    pub password: Secret,
}

#[derive(Debug, Clone)]
pub struct ProxySpec {
    pub kind: ProxyKind,
    pub host: String,
    pub port: u16,
    /// `None` connects without authentication.
    pub credentials: Option<ProxyCredentials>,
}

impl ProxySpec {
    pub fn new(kind: ProxyKind, host: impl Into<String>, port: u16) -> Self {
        Self {
            kind,
            host: host.into(),
            port,
            credentials: None,
        }
    }

    /// `SOCKS5 proxy host:port` / `HTTP proxy host:port`: how errors and the status line name the proxy.
    pub fn describe(&self) -> String {
        let label = match self.kind {
            ProxyKind::Socks5 => "SOCKS5 proxy",
            ProxyKind::HttpConnect => "HTTP proxy",
        };
        format!("{label} {}:{}", self.host, self.port)
    }

    pub(crate) fn validate(&self) -> Result<(), ConfigError> {
        validate_host("proxy host", &self.host, false)?;
        if self.port == 0 {
            return Err(ConfigError::Invalid("proxy port must not be 0".into()));
        }
        let Some(login) = &self.credentials else {
            return Ok(());
        };
        let refuse = |why: &str| Err(ConfigError::Invalid(format!("proxy credentials: {why}")));
        if login.user.chars().any(char::is_control) {
            return refuse("the user name has control characters");
        }
        match self.kind {
            // RFC 1929: both fields are 1 to 255 octets.
            ProxyKind::Socks5 => {
                let (user, secret) = (login.user.len(), login.password.expose().len());
                if user == 0 || secret == 0 {
                    return refuse("SOCKS5 needs a user name and a password");
                }
                if user > 255 || secret > 255 {
                    return refuse("SOCKS5 user name and password are at most 255 bytes each");
                }
            }
            // RFC 7617: the user-id cannot contain a colon.
            ProxyKind::HttpConnect if login.user.contains(':') => {
                return refuse("an HTTP Basic user name cannot contain ':'");
            }
            ProxyKind::HttpConnect => {}
        }
        Ok(())
    }

    fn fail(&self, reason: impl std::fmt::Display) -> TunnelError {
        TunnelError::Proxy {
            proxy: self.describe(),
            reason: reason.to_string(),
        }
    }

    /// Connects to the proxy and asks it for a connection to `host:port`. Unbounded: the caller applies its
    /// own deadline (`Timeouts::connect`). The returned socket carries the target's bytes.
    pub(crate) async fn connect(&self, host: &str, port: u16) -> Result<TcpStream, TunnelError> {
        let tcp = TcpStream::connect((self.host.as_str(), self.port))
            .await
            .map_err(|e| self.fail(format!("cannot connect to the proxy: {e}")))?;
        let _ = tcp.set_nodelay(true);
        self.negotiate(tcp, host, port).await
    }

    /// The protocol exchange on an already open stream to the proxy.
    pub(crate) async fn negotiate<S>(
        &self,
        mut stream: S,
        host: &str,
        port: u16,
    ) -> Result<S, TunnelError>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let outcome = match Destination::parse(host) {
            Ok(destination) => match self.kind {
                ProxyKind::Socks5 => self.socks5(&mut stream, &destination, port).await,
                ProxyKind::HttpConnect => self.http_connect(&mut stream, &destination, port).await,
            },
            Err(reason) => Err(Failure::Refused(reason)),
        };
        match outcome {
            Ok(()) => Ok(stream),
            Err(Failure::Refused(reason)) => {
                Err(self.fail(format!("{reason} (asked for {host}:{port})")))
            }
            Err(Failure::Transport(e)) => Err(self.fail(match e.kind() {
                io::ErrorKind::UnexpectedEof => {
                    "the proxy closed the connection before it answered".to_owned()
                }
                _ => format!("connection to the proxy failed: {e}"),
            })),
        }
    }

    /// RFC 1928: method selection, optional RFC 1929 login, then a CONNECT request.
    async fn socks5<S>(&self, io: &mut S, to: &Destination<'_>, port: u16) -> Result<(), Failure>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        const NO_AUTH: u8 = 0x00;
        const USER_PASS: u8 = 0x02;
        const NO_ACCEPTABLE: u8 = 0xff;

        let login = self.credentials.as_ref();
        let methods: &[u8] = if login.is_some() {
            &[NO_AUTH, USER_PASS]
        } else {
            &[NO_AUTH]
        };
        let mut hello = vec![5, methods.len() as u8];
        hello.extend_from_slice(methods);
        io.write_all(&hello).await?;

        let [version, method] = read_array::<2>(io).await?;
        if version != 5 {
            return Err(Reason::NotSocks5(version).into());
        }
        match (method, login) {
            (NO_AUTH, _) => {}
            (USER_PASS, Some(login)) => {
                // RFC 1929 section 2: VER=1, ULEN, UNAME, PLEN, PASSWD.
                let (user, secret) = (login.user.as_bytes(), login.password.expose().as_bytes());
                let (Ok(ulen), Ok(plen)) = (u8::try_from(user.len()), u8::try_from(secret.len()))
                else {
                    return Err(Reason::LoginTooLong.into());
                };
                let mut message = Zeroizing::new(Vec::with_capacity(3 + user.len() + secret.len()));
                message.push(1);
                message.push(ulen);
                message.extend_from_slice(user);
                message.push(plen);
                message.extend_from_slice(secret);
                io.write_all(&message).await?;
                match read_array::<2>(io).await? {
                    [1, 0] => {}
                    [1, _] => return Err(Reason::LoginRefused.into()),
                    [other, _] => return Err(Reason::LoginReplyVersion(other).into()),
                }
            }
            (NO_ACCEPTABLE, _) => return Err(Reason::NoCommonMethod.into()),
            (other, _) => return Err(Reason::MethodNotOffered(other).into()),
        }

        // Request: VER, CMD=CONNECT, RSV, ATYP, DST.ADDR, DST.PORT.
        let mut request = vec![5, 1, 0];
        to.write_socks5(&mut request)?;
        request.extend_from_slice(&port.to_be_bytes());
        io.write_all(&request).await?;

        let [version, status, _, address_type] = read_array::<4>(io).await?;
        if version != 5 {
            return Err(Reason::NotSocks5(version).into());
        }
        if status != 0 {
            return Err(Reason::Socks5Status(status).into());
        }
        // BND.ADDR and BND.PORT are of no use to us; consume exactly their length.
        let address_len = match address_type {
            1 => 4,
            4 => 16,
            3 => usize::from(read_array::<1>(io).await?[0]),
            other => return Err(Reason::BoundAddressType(other).into()),
        };
        let mut bound = [0u8; 255 + 2];
        io.read_exact(&mut bound[..address_len + 2]).await?;
        Ok(())
    }

    /// RFC 9110 section 9.3.6: `CONNECT authority`, answered by any 2xx once the tunnel is open.
    async fn http_connect<S>(
        &self,
        io: &mut S,
        to: &Destination<'_>,
        port: u16,
    ) -> Result<(), Failure>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let request = connect_request(&to.authority(port), self.credentials.as_ref());
        io.write_all(request.as_bytes()).await?;

        let mut informational = 0;
        loop {
            let head = read_head(io).await?;
            match status_of(&head)? {
                100..=199 if informational < INFORMATIONAL_LIMIT => informational += 1,
                100..=199 => return Err(Reason::TooManyInformational.into()),
                200..=299 => return Ok(()),
                407 => return Err(Reason::HttpLoginNeeded(quote_status(&head)).into()),
                _ => return Err(Reason::HttpDeclined(quote_status(&head)).into()),
            }
        }
    }
}

/// The CONNECT request (RFC 9110 section 9.3.6) with the Basic credentials of RFC 7617 when there are any.
/// Everything that holds the password is sized exactly up front, so no buffer is ever reallocated (which would
/// leave an un-zeroized copy behind) and all of them are wiped on drop.
fn connect_request(authority: &str, login: Option<&ProxyCredentials>) -> Zeroizing<String> {
    const HEAD: &str = " HTTP/1.1\r\nHost: ";
    const AUTH_NAME: &str = "Proxy-Authorization: Basic ";
    let token = login.map(|login| {
        let (user, secret) = (login.user.as_str(), login.password.expose());
        let mut pair = Zeroizing::new(String::with_capacity(user.len() + 1 + secret.len()));
        pair.push_str(user);
        pair.push(':');
        pair.push_str(secret);
        Zeroizing::new(BASE64.encode(pair.as_bytes()))
    });
    let capacity = "CONNECT ".len()
        + authority.len() * 2
        + HEAD.len()
        + 2
        + token.as_ref().map_or(0, |t| AUTH_NAME.len() + t.len() + 2)
        + 2;
    let mut request = Zeroizing::new(String::with_capacity(capacity));
    request.push_str("CONNECT ");
    request.push_str(authority);
    request.push_str(HEAD);
    request.push_str(authority);
    request.push_str("\r\n");
    if let Some(token) = &token {
        request.push_str(AUTH_NAME);
        request.push_str(token);
        request.push_str("\r\n");
    }
    request.push_str("\r\n");
    request
}

/// How an exchange ended badly: the proxy said no / spoke wrongly, or the socket failed.
enum Failure {
    Refused(Reason),
    Transport(io::Error),
}

impl From<Reason> for Failure {
    fn from(reason: Reason) -> Self {
        Self::Refused(reason)
    }
}

impl From<io::Error> for Failure {
    fn from(error: io::Error) -> Self {
        Self::Transport(error)
    }
}

/// What the proxy (or the request we would have to send) got wrong. Never contains a credential.
#[derive(Debug, Error, PartialEq, Eq)]
enum Reason {
    #[error("the answer is not SOCKS5 (version byte {0})")]
    NotSocks5(u8),
    #[error("the proxy supports none of the authentication methods offered")]
    NoCommonMethod,
    #[error("the proxy selected authentication method {0:#04x}, which was not offered")]
    MethodNotOffered(u8),
    #[error("the user name or password is longer than 255 bytes")]
    LoginTooLong,
    #[error("the proxy rejected the user name or password")]
    LoginRefused,
    #[error("unexpected version {0} in the proxy's login answer")]
    LoginReplyVersion(u8),
    #[error("the proxy refused the connection: {}", socks5_status_text(*.0))]
    Socks5Status(u8),
    #[error("unknown address type {0} in the proxy's answer")]
    BoundAddressType(u8),
    #[error("the target is not a plain host name or address")]
    BadTarget,
    #[error("the target name is longer than 255 bytes, the most SOCKS5 can carry")]
    NameTooLong,
    #[error("the proxy refused the connection: {0}")]
    HttpDeclined(String),
    #[error("the proxy wants valid credentials: {0}")]
    HttpLoginNeeded(String),
    #[error("the proxy's answer is not an HTTP/1.x response")]
    NotHttp,
    #[error("the proxy's answer head is longer than {HEAD_LIMIT} bytes")]
    HeadTooLong,
    #[error("the proxy sent more than {INFORMATIONAL_LIMIT} informational (1xx) answers")]
    TooManyInformational,
}

/// RFC 1928 section 6, the REP values other than success.
fn socks5_status_text(status: u8) -> String {
    match status {
        1 => "general SOCKS server failure".into(),
        2 => "connection not allowed by the proxy's rule set".into(),
        3 => "network unreachable".into(),
        4 => "host unreachable".into(),
        5 => "the target refused the connection".into(),
        6 => "TTL expired".into(),
        7 => "command not supported".into(),
        8 => "address type not supported".into(),
        other => format!("unassigned status {other}"),
    }
}

/// Where the proxy should connect to.
enum Destination<'a> {
    V4(Ipv4Addr),
    V6(Ipv6Addr),
    Name(&'a str),
}

impl<'a> Destination<'a> {
    /// A bracketed or bare IP literal is an address; anything else must be a plain name. Whitespace and
    /// control characters are refused here so they can never reach a request line or header.
    fn parse(host: &'a str) -> Result<Self, Reason> {
        let bare = host
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
            .unwrap_or(host);
        match bare.parse::<IpAddr>() {
            Ok(IpAddr::V4(address)) => Ok(Self::V4(address)),
            Ok(IpAddr::V6(address)) => Ok(Self::V6(address)),
            Err(_)
                if bare.is_empty() || bare.chars().any(|c| c.is_control() || c.is_whitespace()) =>
            {
                Err(Reason::BadTarget)
            }
            Err(_) => Ok(Self::Name(bare)),
        }
    }

    /// ATYP and DST.ADDR (RFC 1928 section 5).
    fn write_socks5(&self, out: &mut Vec<u8>) -> Result<(), Reason> {
        match self {
            Self::V4(address) => {
                out.push(1);
                out.extend_from_slice(&address.octets());
            }
            Self::V6(address) => {
                out.push(4);
                out.extend_from_slice(&address.octets());
            }
            Self::Name(name) => {
                out.push(3);
                out.push(u8::try_from(name.len()).map_err(|_| Reason::NameTooLong)?);
                out.extend_from_slice(name.as_bytes());
            }
        }
        Ok(())
    }

    /// The CONNECT request-target (`host:port`, IPv6 in brackets).
    fn authority(&self, port: u16) -> String {
        match self {
            Self::V4(address) => format!("{address}:{port}"),
            Self::V6(address) => format!("[{address}]:{port}"),
            Self::Name(name) => format!("{name}:{port}"),
        }
    }
}

async fn read_array<const N: usize>(io: &mut (impl AsyncRead + Unpin)) -> io::Result<[u8; N]> {
    let mut bytes = [0u8; N];
    io.read_exact(&mut bytes).await?;
    Ok(bytes)
}

/// Reads one response head, up to and including its blank line, and not a byte more (a tunnelled server may
/// have spoken already). One byte per read is fine: it happens once per connection.
async fn read_head(io: &mut (impl AsyncRead + Unpin)) -> Result<Vec<u8>, Failure> {
    let mut head = Vec::with_capacity(128);
    while head.len() < HEAD_LIMIT {
        head.push(read_array::<1>(io).await?[0]);
        if head.ends_with(b"\r\n\r\n") || head.ends_with(b"\n\n") {
            return Ok(head);
        }
    }
    Err(Reason::HeadTooLong.into())
}

fn status_line(head: &[u8]) -> &[u8] {
    let line = head.split(|b| *b == b'\n').next().unwrap_or_default();
    line.strip_suffix(b"\r").unwrap_or(line)
}

/// The status code of `HTTP/1.x SSS ...`.
fn status_of(head: &[u8]) -> Result<u16, Reason> {
    let line = std::str::from_utf8(status_line(head)).map_err(|_| Reason::NotHttp)?;
    let mut fields = line.splitn(3, ' ');
    let (Some(version), Some(code)) = (fields.next(), fields.next()) else {
        return Err(Reason::NotHttp);
    };
    if !matches!(version, "HTTP/1.0" | "HTTP/1.1") || code.len() != 3 {
        return Err(Reason::NotHttp);
    }
    code.parse().map_err(|_| Reason::NotHttp)
}

/// The status line as text that is safe to put in a message: control characters and bidirectional
/// overrides dropped, length capped.
fn quote_status(head: &[u8]) -> String {
    let hidden = |c: &char| {
        c.is_control()
            || matches!(*c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}' | '\u{2066}'..='\u{2069}' | '\u{feff}')
    };
    String::from_utf8_lossy(status_line(head))
        .chars()
        .filter(|c| !hidden(c))
        .take(QUOTE_LIMIT)
        .collect()
}

// ---- proxy-only route ------------------------------------------------------------------------------------

/// A tunnel without SSH: every connection to a local listener is sent through the proxy to its target.
/// Open it with [`Tunnel::open_proxy`](crate::Tunnel::open_proxy).
#[derive(Debug, Clone)]
pub struct ProxyRoute {
    pub proxy: ProxySpec,
    /// One local listener per distinct target.
    pub targets: Vec<Target>,
    /// Only `connect` (the proxy exchange of each forward) and the buffer / stall timeouts apply.
    pub timeouts: Timeouts,
    pub max_concurrent_forwards: usize,
    pub forward_buffer_bytes: usize,
    pub tunnel_buffer_bytes: usize,
}

impl ProxyRoute {
    pub fn new(proxy: ProxySpec) -> Self {
        Self {
            proxy,
            targets: Vec::new(),
            timeouts: Timeouts::default(),
            max_concurrent_forwards: DEFAULT_MAX_CONCURRENT_FORWARDS,
            forward_buffer_bytes: DEFAULT_FORWARD_BUFFER_BYTES,
            tunnel_buffer_bytes: DEFAULT_TUNNEL_BUFFER_BYTES,
        }
    }

    /// Checks everything that can be checked without the network.
    pub fn validate(&self) -> Result<(), TunnelError> {
        self.proxy.validate()?;
        validate_targets(&self.targets)?;
        validate_limits(
            &self.timeouts,
            self.forward_buffer_bytes,
            self.tunnel_buffer_bytes,
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{duplex, DuplexStream};

    fn login() -> ProxyCredentials {
        ProxyCredentials {
            user: "alice".into(),
            password: Secret::new("pw-needle-9"),
        }
    }

    fn proxy(kind: ProxyKind, with_login: bool) -> ProxySpec {
        let mut spec = ProxySpec::new(kind, "proxy.example.com", 1080);
        spec.credentials = with_login.then(login);
        spec
    }

    /// Runs the client side against `script`, which plays the proxy on the other end of a pipe and returns
    /// everything it read.
    async fn exchange<F, Fut>(
        spec: &ProxySpec,
        script: F,
    ) -> (Result<DuplexStream, TunnelError>, Vec<u8>)
    where
        F: FnOnce(DuplexStream) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Vec<u8>> + Send + 'static,
    {
        let (ours, theirs) = duplex(64 * 1024);
        let server = tokio::spawn(script(theirs));
        let outcome = spec.negotiate(ours, "db.internal", 5432).await;
        (outcome, server.await.unwrap())
    }

    async fn take(io: &mut DuplexStream, n: usize) -> Vec<u8> {
        let mut bytes = vec![0; n];
        io.read_exact(&mut bytes).await.unwrap();
        bytes
    }

    fn proxy_error(outcome: Result<DuplexStream, TunnelError>) -> String {
        match outcome {
            Err(error @ TunnelError::Proxy { .. }) => {
                assert_eq!(error.code(), "tunnel.proxy");
                error.to_string()
            }
            other => panic!("expected a proxy error, got {other:?}"),
        }
    }

    #[test]
    fn socks5_destinations_follow_rfc_1928_address_types() {
        let encode = |host: &str| {
            let mut out = Vec::new();
            Destination::parse(host)
                .unwrap()
                .write_socks5(&mut out)
                .map(|()| out)
        };
        assert_eq!(encode("10.1.2.3").unwrap(), [1, 10, 1, 2, 3]);
        let v6 = encode("[2001:db8::1]").unwrap();
        assert_eq!((v6[0], v6.len()), (4, 17));
        assert_eq!(encode("::1").unwrap()[0], 4);
        assert_eq!(
            encode("db.internal").unwrap(),
            [&[3u8, 11][..], b"db.internal"].concat()
        );
        assert_eq!(encode(&"a".repeat(255)).unwrap()[1], 255);
        assert_eq!(encode(&"a".repeat(256)), Err(Reason::NameTooLong));
        for bad in ["", "a b", "a\r\nb", "tab\t"] {
            assert!(
                matches!(Destination::parse(bad), Err(Reason::BadTarget)),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn the_connect_request_is_sized_exactly_so_the_token_is_never_copied() {
        for credentials in [None, Some(login())] {
            let request = connect_request("db.internal:5432", credentials.as_ref());
            assert_eq!(
                request.capacity(),
                request.len(),
                "no reallocation, no spare room"
            );
            assert!(request
                .starts_with("CONNECT db.internal:5432 HTTP/1.1\r\nHost: db.internal:5432\r\n"));
            assert!(request.ends_with("\r\n\r\n"));
            assert_eq!(
                request.contains("Proxy-Authorization"),
                credentials.is_some()
            );
        }
        // A long password (token longer than the head) is sized just as exactly.
        let long = ProxyCredentials {
            user: "u".into(),
            password: Secret::new("p".repeat(250)),
        };
        let request = connect_request("[2001:db8::1]:5432", Some(&long));
        assert_eq!(request.capacity(), request.len());
    }

    #[test]
    fn connect_authorities_bracket_only_ipv6() {
        let authority = |host: &str| Destination::parse(host).unwrap().authority(5432);
        assert_eq!(authority("db.internal"), "db.internal:5432");
        assert_eq!(authority("10.0.0.1"), "10.0.0.1:5432");
        assert_eq!(authority("::1"), "[::1]:5432");
        assert_eq!(authority("[::1]"), "[::1]:5432");
    }

    #[test]
    fn every_assigned_socks5_status_has_its_own_text() {
        let texts: Vec<String> = (1..=8).map(socks5_status_text).collect();
        for (i, text) in texts.iter().enumerate() {
            assert!(!text.contains("unassigned"), "{}: {text}", i + 1);
            assert_eq!(texts.iter().filter(|t| *t == text).count(), 1, "{text}");
        }
        assert!(socks5_status_text(0x7f).contains("unassigned status 127"));
    }

    #[test]
    fn status_parsing_accepts_only_http_1_and_quotes_safely() {
        assert_eq!(
            status_of(b"HTTP/1.1 200 Connection established\r\n\r\n"),
            Ok(200)
        );
        assert_eq!(status_of(b"HTTP/1.0 407 x\n\n"), Ok(407));
        assert_eq!(status_of(b"HTTP/1.1 200\r\n\r\n"), Ok(200));
        for bad in [
            &b"garbage\r\n\r\n"[..],
            b"HTTP/2 200 OK\r\n\r\n",
            b"HTTP/1.1 20 OK\r\n\r\n",
            b"HTTP/1.1 2000 OK\r\n\r\n",
            b"HTTP/1.1 abc OK\r\n\r\n",
            b"\xff\xfe\r\n\r\n",
            b"\r\n\r\n",
        ] {
            assert_eq!(status_of(bad), Err(Reason::NotHttp), "{bad:?}");
        }
        assert_eq!(
            quote_status(b"HTTP/1.1 403 For\x1b[31mbidden \xe2\x80\xae\x07evil\r\n\r\n"),
            "HTTP/1.1 403 For[31mbidden evil"
        );
        let long = format!("HTTP/1.1 500 {}\r\n\r\n", "x".repeat(5000));
        assert_eq!(quote_status(long.as_bytes()).chars().count(), QUOTE_LIMIT);
    }

    #[tokio::test]
    async fn socks5_without_login_leaves_what_follows_the_reply_unread() {
        let (outcome, seen) = exchange(&proxy(ProxyKind::Socks5, false), |mut s| async move {
            let mut seen = take(&mut s, 3).await;
            s.write_all(&[5, 0]).await.unwrap();
            seen.extend(take(&mut s, 4 + 1 + 11 + 2).await);
            // A bound *domain* address, then at once the tunnelled protocol's first bytes.
            s.write_all(&[5, 0, 0, 3, 4]).await.unwrap();
            s.write_all(b"host\x00\x50PGHELLO").await.unwrap();
            seen
        })
        .await;
        assert_eq!(&seen[..3], &[5, 1, 0]);
        let mut stream = outcome.unwrap();
        assert_eq!(take_from(&mut stream, 7).await, b"PGHELLO");
    }

    async fn take_from(io: &mut DuplexStream, n: usize) -> Vec<u8> {
        take(io, n).await
    }

    #[tokio::test]
    async fn socks5_login_is_rfc_1929_and_rejection_is_typed() {
        let (outcome, seen) = exchange(&proxy(ProxyKind::Socks5, true), |mut s| async move {
            let mut seen = take(&mut s, 4).await;
            s.write_all(&[5, 2]).await.unwrap();
            seen.extend(take(&mut s, 2 + 5 + 1 + 11).await);
            s.write_all(&[1, 0]).await.unwrap();
            seen.extend(take(&mut s, 4 + 1 + 11 + 2).await);
            s.write_all(&[5, 0, 0, 1, 1, 2, 3, 4, 0, 80]).await.unwrap();
            seen
        })
        .await;
        outcome.unwrap();
        assert_eq!(
            &seen[..4],
            &[5, 2, 0, 2],
            "no-auth and user/password offered"
        );
        assert_eq!(
            &seen[4..],
            &[
                &[1u8, 5][..],
                b"alice",
                &[11],
                b"pw-needle-9",
                &[5, 1, 0, 3, 11],
                b"db.internal",
                &5432u16.to_be_bytes()
            ]
            .concat()[..]
        );

        let (outcome, _) = exchange(&proxy(ProxyKind::Socks5, true), |mut s| async move {
            take(&mut s, 4).await;
            s.write_all(&[5, 2]).await.unwrap();
            take(&mut s, 2 + 5 + 1 + 11).await;
            s.write_all(&[1, 1]).await.unwrap();
            Vec::new()
        })
        .await;
        let text = proxy_error(outcome);
        assert!(
            text.starts_with("SOCKS5 proxy proxy.example.com:1080: "),
            "{text}"
        );
        assert!(
            text.contains("rejected the user name or password"),
            "{text}"
        );
        assert!(
            !text.contains("pw-needle-9") && !text.contains("alice"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn socks5_failures_are_typed() {
        for status in 1..=8u8 {
            let (outcome, _) =
                exchange(&proxy(ProxyKind::Socks5, false), move |mut s| async move {
                    take(&mut s, 3).await;
                    s.write_all(&[5, 0]).await.unwrap();
                    take(&mut s, 4 + 1 + 11 + 2).await;
                    s.write_all(&[5, status, 0, 1, 0, 0, 0, 0, 0, 0])
                        .await
                        .unwrap();
                    Vec::new()
                })
                .await;
            let text = proxy_error(outcome);
            assert!(
                text.contains("db.internal:5432") && !text.contains("unassigned"),
                "{text}"
            );
        }
        let cases: [(&[u8], &str); 4] = [
            (&[5, 0xff], "none of the authentication methods"),
            (&[5, 2], "not offered"),
            (b"HT", "not SOCKS5"),
            (&[], "closed the connection"),
        ];
        for (answer, needle) in cases {
            let answer = answer.to_vec();
            let (outcome, _) =
                exchange(&proxy(ProxyKind::Socks5, false), move |mut s| async move {
                    take(&mut s, 3).await;
                    s.write_all(&answer).await.unwrap();
                    Vec::new()
                })
                .await;
            assert!(proxy_error(outcome).contains(needle), "{needle}");
        }
        // An address type the RFC does not define.
        let (outcome, _) = exchange(&proxy(ProxyKind::Socks5, false), |mut s| async move {
            take(&mut s, 3).await;
            s.write_all(&[5, 0]).await.unwrap();
            take(&mut s, 4 + 1 + 11 + 2).await;
            s.write_all(&[5, 0, 0, 9]).await.unwrap();
            Vec::new()
        })
        .await;
        assert!(proxy_error(outcome).contains("unknown address type 9"));
    }

    async fn serve_head(s: &mut DuplexStream) -> Vec<u8> {
        let mut seen = Vec::new();
        while !seen.ends_with(b"\r\n\r\n") {
            seen.extend(take(s, 1).await);
        }
        seen
    }

    #[tokio::test]
    async fn http_connect_request_and_success_keep_the_following_bytes() {
        for with_login in [false, true] {
            let (outcome, seen) = exchange(
                &proxy(ProxyKind::HttpConnect, with_login),
                |mut s| async move {
                    let seen = serve_head(&mut s).await;
                    s.write_all(
                        b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nX: y\r\n\r\nRESP",
                    )
                    .await
                    .unwrap();
                    seen
                },
            )
            .await;
            let mut stream = outcome.unwrap();
            assert_eq!(take_from(&mut stream, 4).await, b"RESP");
            let text = String::from_utf8(seen).unwrap();
            assert!(
                text.starts_with("CONNECT db.internal:5432 HTTP/1.1\r\nHost: db.internal:5432\r\n")
            );
            assert!(text.ends_with("\r\n\r\n"));
            let expected = format!(
                "Proxy-Authorization: Basic {}\r\n",
                BASE64.encode("alice:pw-needle-9")
            );
            assert_eq!(text.contains(&expected), with_login, "{text}");
            assert_eq!(text.contains("Proxy-Authorization"), with_login);
        }
    }

    async fn answer_with(bytes: Vec<u8>) -> String {
        let (outcome, _) = exchange(
            &proxy(ProxyKind::HttpConnect, false),
            move |mut s| async move {
                serve_head(&mut s).await;
                let _ = s.write_all(&bytes).await;
                Vec::new()
            },
        )
        .await;
        proxy_error(outcome)
    }

    #[tokio::test]
    async fn http_connect_refusals_and_malformed_answers() {
        for status in [400, 401, 403, 404, 405, 500, 502, 503, 504, 301] {
            let text = answer_with(format!("HTTP/1.1 {status} Nope\r\n\r\n").into_bytes()).await;
            assert!(text.contains(&format!("HTTP/1.1 {status} Nope")), "{text}");
        }
        let text =
            answer_with(b"HTTP/1.1 407 Proxy Authentication Required\r\n\r\n".to_vec()).await;
        assert!(text.contains("wants valid credentials"), "{text}");
        let text = answer_with(b"HTTP/1.1 403 \x1b[2Jcleared\x07\r\n\r\n".to_vec()).await;
        assert!(
            text.contains("HTTP/1.1 403 [2Jcleared") && !text.contains('\x1b'),
            "{text:?}"
        );
        let mut huge = b"HTTP/1.1 200 OK\r\n".to_vec();
        huge.extend(std::iter::repeat_n(b'a', 20_000));
        assert!(answer_with(huge).await.contains("longer than 8192"));
        assert!(answer_with(b"SSH-2.0-OpenSSH_9\r\n\r\n".to_vec())
            .await
            .contains("not an HTTP/1.x"));
        assert!(answer_with(b"HTTP/1.1 200".to_vec())
            .await
            .contains("closed the connection"));
        assert!(answer_with(b"HTTP/1.1 102 P\r\n\r\n".repeat(6))
            .await
            .contains("informational"));
    }

    #[test]
    fn validation_covers_host_port_and_credentials() {
        proxy(ProxyKind::Socks5, true).validate().unwrap();
        proxy(ProxyKind::HttpConnect, true).validate().unwrap();
        let problem = |kind, edit: &dyn Fn(&mut ProxySpec)| {
            let mut spec = proxy(kind, true);
            edit(&mut spec);
            spec.validate().unwrap_err().to_string()
        };
        fn login(spec: &mut ProxySpec) -> &mut ProxyCredentials {
            spec.credentials.as_mut().unwrap()
        }
        assert!(problem(ProxyKind::Socks5, &|p| p.host = "a b".into()).contains("proxy host"));
        assert!(problem(ProxyKind::Socks5, &|p| p.port = 0).contains("port"));
        assert!(problem(ProxyKind::Socks5, &|p| login(p).user.clear())
            .contains("user name and a password"));
        assert!(problem(ProxyKind::Socks5, &|p| login(p).user = "u".repeat(256)).contains("255"));
        assert!(problem(ProxyKind::HttpConnect, &|p| login(p).user = "a:b".into()).contains("':'"));
        assert!(
            problem(ProxyKind::HttpConnect, &|p| login(p).user = "a\nb".into()).contains("control")
        );
    }

    #[test]
    fn names_and_debug_output_never_show_the_password() {
        assert_eq!(
            proxy(ProxyKind::Socks5, true).describe(),
            "SOCKS5 proxy proxy.example.com:1080"
        );
        assert_eq!(
            proxy(ProxyKind::HttpConnect, false).describe(),
            "HTTP proxy proxy.example.com:1080"
        );
        let shown = format!(
            "{:?} {:#?}",
            proxy(ProxyKind::HttpConnect, true),
            proxy(ProxyKind::Socks5, true)
        );
        assert!(!shown.contains("pw-needle-9"), "{shown}");
    }
}
