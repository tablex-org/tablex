//! In-process SOCKS5 and HTTP CONNECT proxies for tests (fixtures, not product code). They listen on 127.0.0.1
//! on their own runtime, can require a login, can refuse every request with a chosen code / status, and record
//! what they were asked for (so a test can prove the target name reached the proxy unresolved).
#![allow(dead_code)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::runtime::{Builder, Runtime};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Protocol {
    Socks5,
    Http,
}

#[derive(Clone, Debug)]
pub enum Refusal {
    /// SOCKS5 reply code (1..=8).
    Socks5(u8),
    /// HTTP status line, e.g. `403 Forbidden`.
    Http(&'static str),
    /// Accept the TCP connection and never answer.
    Silent,
}

#[derive(Clone)]
pub struct ProxyOptions {
    pub protocol: Protocol,
    /// `(user, password)` the proxy demands; `None` accepts anyone.
    pub login: Option<(String, String)>,
    pub refuse: Option<Refusal>,
    /// Names the proxy resolves itself: `(host, port)` as asked -> where it connects. Anything else is
    /// connected to as asked.
    pub resolve: HashMap<(String, u16), SocketAddr>,
}

impl ProxyOptions {
    pub fn new(protocol: Protocol) -> Self {
        Self {
            protocol,
            login: None,
            refuse: None,
            resolve: HashMap::new(),
        }
    }

    pub fn with_login(mut self, user: &str, password: &str) -> Self {
        self.login = Some((user.into(), password.into()));
        self
    }

    pub fn refusing(mut self, refusal: Refusal) -> Self {
        self.refuse = Some(refusal);
        self
    }

    pub fn resolving(mut self, host: &str, port: u16, to: SocketAddr) -> Self {
        self.resolve.insert((host.into(), port), to);
        self
    }
}

#[derive(Default)]
pub struct ProxyStats {
    pub connections: AtomicUsize,
    /// Successful tunnels set up.
    pub tunnels: AtomicUsize,
    /// Logins / `Proxy-Authorization` headers that were wrong or missing.
    pub rejected_logins: AtomicUsize,
    /// Every request: `domain:name:port` (SOCKS5 ATYP 3), `ip:addr:port`, or `connect:authority` (HTTP).
    pub requests: Mutex<Vec<String>>,
    /// The raw `Proxy-Authorization` header values seen (HTTP).
    pub authorizations: Mutex<Vec<String>>,
}

pub struct TestProxy {
    pub addr: SocketAddr,
    pub stats: Arc<ProxyStats>,
    runtime: Option<Runtime>,
}

impl TestProxy {
    pub fn start(options: ProxyOptions) -> Self {
        let runtime = Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let listener = runtime.block_on(TcpListener::bind("127.0.0.1:0")).unwrap();
        let addr = listener.local_addr().unwrap();
        let stats = Arc::new(ProxyStats::default());
        let (accept_stats, options) = (stats.clone(), Arc::new(options));
        runtime.spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                accept_stats.connections.fetch_add(1, Ordering::SeqCst);
                let (stats, options) = (accept_stats.clone(), options.clone());
                tokio::spawn(async move {
                    let _ = match options.protocol {
                        Protocol::Socks5 => serve_socks5(stream, &options, &stats).await,
                        Protocol::Http => serve_http(stream, &options, &stats).await,
                    };
                });
            }
        });
        Self {
            addr,
            stats,
            runtime: Some(runtime),
        }
    }

    pub fn requests(&self) -> Vec<String> {
        self.stats.requests.lock().unwrap().clone()
    }

    pub fn tunnels(&self) -> usize {
        self.stats.tunnels.load(Ordering::SeqCst)
    }
}

impl Drop for TestProxy {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

async fn connect_target(
    options: &ProxyOptions,
    host: &str,
    port: u16,
) -> std::io::Result<TcpStream> {
    match options.resolve.get(&(host.to_owned(), port)) {
        Some(addr) => TcpStream::connect(addr).await,
        None => TcpStream::connect((host, port)).await,
    }
}

async fn relay(mut client: TcpStream, mut target: TcpStream) {
    let _ = tokio::io::copy_bidirectional(&mut client, &mut target).await;
}

async fn serve_socks5(
    mut stream: TcpStream,
    options: &ProxyOptions,
    stats: &ProxyStats,
) -> std::io::Result<()> {
    if matches!(options.refuse, Some(Refusal::Silent)) {
        tokio::time::sleep(Duration::from_secs(120)).await;
        return Ok(());
    }
    let mut head = [0u8; 2];
    stream.read_exact(&mut head).await?;
    let mut methods = vec![0u8; head[1] as usize];
    stream.read_exact(&mut methods).await?;
    match &options.login {
        Some((user, password)) => {
            if !methods.contains(&0x02) {
                stream.write_all(&[5, 0xff]).await?;
                return Ok(());
            }
            stream.write_all(&[5, 2]).await?;
            let mut version = [0u8; 2];
            stream.read_exact(&mut version).await?;
            let mut got_user = vec![0u8; version[1] as usize];
            stream.read_exact(&mut got_user).await?;
            let mut plen = [0u8; 1];
            stream.read_exact(&mut plen).await?;
            let mut got_password = vec![0u8; plen[0] as usize];
            stream.read_exact(&mut got_password).await?;
            if got_user != user.as_bytes() || got_password != password.as_bytes() {
                stats.rejected_logins.fetch_add(1, Ordering::SeqCst);
                stream.write_all(&[1, 1]).await?;
                return Ok(());
            }
            stream.write_all(&[1, 0]).await?;
        }
        None => {
            if !methods.contains(&0x00) {
                stream.write_all(&[5, 0xff]).await?;
                return Ok(());
            }
            stream.write_all(&[5, 0]).await?;
        }
    }
    let mut request = [0u8; 4];
    stream.read_exact(&mut request).await?;
    let (host, description) = match request[3] {
        0x01 => {
            let mut octets = [0u8; 4];
            stream.read_exact(&mut octets).await?;
            let ip = std::net::Ipv4Addr::from(octets).to_string();
            (ip.clone(), format!("ip:{ip}"))
        }
        0x04 => {
            let mut octets = [0u8; 16];
            stream.read_exact(&mut octets).await?;
            let ip = std::net::Ipv6Addr::from(octets).to_string();
            (ip.clone(), format!("ip:{ip}"))
        }
        _ => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).await?;
            let mut name = vec![0u8; len[0] as usize];
            stream.read_exact(&mut name).await?;
            let name = String::from_utf8_lossy(&name).into_owned();
            (name.clone(), format!("domain:{name}"))
        }
    };
    let mut port = [0u8; 2];
    stream.read_exact(&mut port).await?;
    let port = u16::from_be_bytes(port);
    stats
        .requests
        .lock()
        .unwrap()
        .push(format!("{description}:{port}"));
    if let Some(Refusal::Socks5(code)) = options.refuse {
        stream.write_all(&[5, code, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
        return Ok(());
    }
    match connect_target(options, &host, port).await {
        Ok(target) => {
            stats.tunnels.fetch_add(1, Ordering::SeqCst);
            stream.write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0]).await?;
            relay(stream, target).await;
        }
        Err(_) => stream.write_all(&[5, 5, 0, 1, 0, 0, 0, 0, 0, 0]).await?,
    }
    Ok(())
}

async fn serve_http(
    mut stream: TcpStream,
    options: &ProxyOptions,
    stats: &ProxyStats,
) -> std::io::Result<()> {
    if matches!(options.refuse, Some(Refusal::Silent)) {
        tokio::time::sleep(Duration::from_secs(120)).await;
        return Ok(());
    }
    let mut header = Vec::new();
    let mut byte = [0u8; 1];
    while !header.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).await?;
        header.push(byte[0]);
        if header.len() > 16 * 1024 {
            return Ok(());
        }
    }
    let text = String::from_utf8_lossy(&header).into_owned();
    let mut lines = text.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let authority = request_line
        .strip_prefix("CONNECT ")
        .and_then(|rest| rest.split(' ').next())
        .unwrap_or_default()
        .to_owned();
    let authorization = lines
        .clone()
        .find_map(|line| line.strip_prefix("Proxy-Authorization: "))
        .map(str::to_owned);
    if let Some(value) = &authorization {
        stats.authorizations.lock().unwrap().push(value.clone());
    }
    stats
        .requests
        .lock()
        .unwrap()
        .push(format!("connect:{authority}"));
    if let Some((user, password)) = &options.login {
        let expected = format!("Basic {}", BASE64.encode(format!("{user}:{password}")));
        if authorization.as_deref() != Some(expected.as_str()) {
            stats.rejected_logins.fetch_add(1, Ordering::SeqCst);
            stream
                .write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"t\"\r\n\r\n")
                .await?;
            return Ok(());
        }
    }
    if let Some(Refusal::Http(status)) = options.refuse {
        stream
            .write_all(format!("HTTP/1.1 {status}\r\n\r\n").as_bytes())
            .await?;
        return Ok(());
    }
    let Some((host, port)) = authority.rsplit_once(':') else {
        stream
            .write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n")
            .await?;
        return Ok(());
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let Ok(port) = port.parse::<u16>() else {
        stream
            .write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n")
            .await?;
        return Ok(());
    };
    match connect_target(options, host, port).await {
        Ok(target) => {
            stats.tunnels.fetch_add(1, Ordering::SeqCst);
            stream
                .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                .await?;
            relay(stream, target).await;
        }
        Err(_) => {
            stream
                .write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n")
                .await?
        }
    }
    Ok(())
}
