//! The Streamable HTTP transport: rmcp's server transport behind a small hyper listener that does
//! the checks rmcp leaves to its host.
//!
//! Every request goes through, in this order:
//! 1. the connection cap and the header timeout (a client that holds a connection without sending
//!    a request is dropped);
//! 2. the bearer token ([`Token::matches`], constant time): a missing or wrong one is a bare `401`
//!    with no body, whatever the path, method or other headers, and nothing else is looked at;
//! 3. the path (`/mcp` only) and the body, read here under the size cap and a time limit (`413`,
//!    `408`);
//! 4. for an `initialize` (the only request that makes a session): the session cap (`503`);
//! 5. rmcp: the `Host` and `Origin` checks against DNS rebinding (`403`), the protocol itself, the
//!    sessions and their idle timeout.
//!
//! **Sessions.** An `initialize` makes a session with a handler of its own
//! ([`McpServer::for_session`]): the handshake state, and the client whose elicitation answers a
//! write of that session, are the session's, while the tool layer and the budget of running calls
//! are shared. The approver reaches the client through the peer of the call being run, so a
//! question goes to the session that made the call and to no other.
//!
//! A client of the newest protocol revision (no `initialize`, no session) is served per request and
//! declares no elicitation capability, so a write through it is put to the user in the tablex app
//! (the approver's fallback, the same as for a stdio client that cannot ask) or, with the app not
//! running, refused with the approver's own message: nothing is run unasked.
//!
//! **Owners.** Every handler of [`McpServer::for_session`] is a client of its own for the tool layer
//! (see `ToolLayer::call_as`): the `tx-` and `ses-` ids one session gets are no use to another.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::{header, HeaderMap, Method, Request, Response, StatusCode};
use http_body_util::{combinators::BoxBody, BodyExt, Full, LengthLimitError, Limited};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use tokio::net::TcpListener;
use tokio::task::JoinSet;

use crate::server::{McpServer, Transport};
use crate::token::Token;
use crate::transport::{close_down, sweep_idle_transactions, ServeError, MAX_FRAME_BYTES};

/// The only path served.
pub const ENDPOINT: &str = "/mcp";

/// The largest request body: one JSON-RPC message, the same cap as a line on stdio.
pub const MAX_BODY_BYTES: usize = MAX_FRAME_BYTES;

/// Sessions open at once. Each is cheap, but each may hold open questions and calls.
pub const MAX_SESSIONS: usize = 16;

/// Connections open at once (a session holds one for its event stream, a call one more).
pub const MAX_CONNECTIONS: usize = 64;

/// A session nobody has sent anything on, or been sent anything on, for this long is closed; its
/// client gets `404` on its next request and starts a new one.
pub const SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// How long a client has to send the headers of a request.
pub const HEADER_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a client has to send the body of a request.
pub const BODY_TIMEOUT: Duration = Duration::from_secs(30);

/// Names the loopback interface goes by in a `Host` header (any port).
const LOOPBACK_HOSTS: [&str; 3] = ["localhost", "127.0.0.1", "[::1]"];

/// Browser origins of the loopback interface (any port), the only ones allowed unless more are
/// added: a page served from the user's own machine, never a page from the internet.
const LOOPBACK_ORIGINS: [&str; 3] = [
    "http://localhost:*",
    "http://127.0.0.1:*",
    "http://[::1]:*",
];

type Body = BoxBody<Bytes, Infallible>;

/// What the HTTP server needs besides the handler.
pub struct HttpConfig {
    pub token: Token,
    /// `Host` values accepted besides the loopback names and the address listened on (a name or
    /// `name:port`); only for `--allow-remote`.
    pub extra_hosts: Vec<String>,
    /// `Origin` values accepted besides the loopback ones (`scheme://host:port`, `:*` for any
    /// port). A request without an `Origin` (every non-browser client) always passes this check.
    pub extra_origins: Vec<String>,
    pub max_body_bytes: usize,
    pub max_sessions: usize,
    pub max_connections: usize,
    pub idle_timeout: Duration,
    pub header_timeout: Duration,
    pub body_timeout: Duration,
}

impl HttpConfig {
    pub fn new(token: Token) -> Self {
        Self {
            token,
            extra_hosts: Vec::new(),
            extra_origins: Vec::new(),
            max_body_bytes: MAX_BODY_BYTES,
            max_sessions: MAX_SESSIONS,
            max_connections: MAX_CONNECTIONS,
            idle_timeout: SESSION_IDLE_TIMEOUT,
            header_timeout: HEADER_TIMEOUT,
            body_timeout: BODY_TIMEOUT,
        }
    }
}

/// Whether `addr` is on this machine only (an IPv4-mapped IPv6 address counts as its IPv4).
pub fn is_loopback(addr: &SocketAddr) -> bool {
    addr.ip().to_canonical().is_loopback()
}

/// The `Host` values to accept when listening on `local`: the loopback names, the address itself
/// when it is a specific one, and `extra`. Never empty (rmcp reads an empty list as "any host").
fn allowed_hosts(local: &SocketAddr, extra: &[String]) -> Vec<String> {
    let mut hosts: Vec<String> = LOOPBACK_HOSTS.iter().map(|host| (*host).to_owned()).collect();
    if !local.ip().is_unspecified() && !is_loopback(local) {
        hosts.push(match local.ip() {
            std::net::IpAddr::V6(ip) => format!("[{ip}]"),
            ip => ip.to_string(),
        });
    }
    hosts.extend(extra.iter().cloned());
    hosts
}

/// What to tell the user at start when the server listens on every interface (`0.0.0.0`, `::`):
/// the machine's own addresses and names are not allowed `Host` values then (only the loopback
/// names and `extra`), so a client that reaches it by its LAN address or host name gets `403`
/// until that name is in the list. `None` for any other address.
pub fn wildcard_hosts_note(local: &SocketAddr, extra: &[String], env_name: &str) -> Option<String> {
    if !local.ip().is_unspecified() {
        return None;
    }
    let accepted = if extra.is_empty() {
        "only localhost, 127.0.0.1 and [::1]".to_owned()
    } else {
        format!("only localhost, 127.0.0.1, [::1] and {}", extra.join(", "))
    };
    Some(format!(
        "listening on every interface: the Host header must be {accepted} (this machine's own addresses and names are not accepted). \
         List the host names clients use in {env_name} (comma separated, `name` or `name:port`), or they get 403"
    ))
}

/// The `Origin` values to accept: the loopback ones and `extra`. Never empty (rmcp turns
/// the check off for an empty list).
fn allowed_origins(extra: &[String]) -> Vec<String> {
    LOOPBACK_ORIGINS
        .iter()
        .map(|origin| (*origin).to_owned())
        .chain(extra.iter().cloned())
        .collect()
}

struct State {
    token: Token,
    service: StreamableHttpService<McpServer, LocalSessionManager>,
    sessions: Arc<LocalSessionManager>,
    /// Held from the session count to the session's creation, so that two `initialize`s cannot
    /// both pass the cap.
    admission: tokio::sync::Mutex<()>,
    max_body_bytes: usize,
    max_sessions: usize,
    body_timeout: Duration,
}

fn reply(status: StatusCode) -> Response<Body> {
    let mut response = Response::new(Full::new(Bytes::new()).boxed());
    *response.status_mut() = status;
    response
}

/// The token in `Authorization: Bearer <token>`, if the header is that.
fn bearer(headers: &HeaderMap) -> Option<&[u8]> {
    let value = headers.get(header::AUTHORIZATION)?.as_bytes();
    let (scheme, token) = value.split_at_checked(7)?;
    scheme.eq_ignore_ascii_case(b"bearer ").then_some(token)
}

/// A bare `401`: it says nothing about why (a missing header, another scheme and a wrong token
/// look the same).
fn unauthorized() -> Response<Body> {
    let mut response = reply(StatusCode::UNAUTHORIZED);
    response
        .headers_mut()
        .insert(header::WWW_AUTHENTICATE, header::HeaderValue::from_static("Bearer"));
    response
}

/// Whether `body` is an `initialize` request: the only message that makes a session.
fn is_initialize(body: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(body)
        .is_ok_and(|message| message.get("method").and_then(|m| m.as_str()) == Some("initialize"))
}

async fn handle(state: Arc<State>, request: Request<Incoming>) -> Result<Response<Body>, Infallible> {
    Ok(route(&state, request).await)
}

async fn route(state: &State, request: Request<Incoming>) -> Response<Body> {
    if !bearer(request.headers()).is_some_and(|presented| state.token.matches(presented)) {
        return unauthorized();
    }
    if request.uri().path() != ENDPOINT {
        return reply(StatusCode::NOT_FOUND);
    }
    let (parts, body) = request.into_parts();
    // The declared size is enough to refuse before reading; the cap on the read is what holds.
    let declared = parts
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok());
    if declared.is_some_and(|size| size > state.max_body_bytes) {
        return reply(StatusCode::PAYLOAD_TOO_LARGE);
    }
    let body = match tokio::time::timeout(
        state.body_timeout,
        Limited::new(body, state.max_body_bytes).collect(),
    )
    .await
    {
        Ok(Ok(collected)) => collected.to_bytes(),
        Ok(Err(error)) if error.downcast_ref::<LengthLimitError>().is_some() => {
            return reply(StatusCode::PAYLOAD_TOO_LARGE)
        }
        Ok(Err(_)) => return reply(StatusCode::BAD_REQUEST),
        Err(_) => return reply(StatusCode::REQUEST_TIMEOUT),
    };
    let creates_session = parts.method == Method::POST
        && !parts.headers.contains_key("mcp-session-id")
        && is_initialize(&body);
    let _admission = if creates_session {
        let admission = state.admission.lock().await;
        if state.sessions.sessions.read().await.len() >= state.max_sessions {
            return reply(StatusCode::SERVICE_UNAVAILABLE);
        }
        Some(admission)
    } else {
        None
    };
    state
        .service
        .handle(Request::from_parts(parts, Full::new(body)))
        .await
}

impl McpServer {
    /// Serves MCP over Streamable HTTP on `listener` until `stop` completes (a termination signal),
    /// then cancels what is still running and closes every session of the tool layer, bounded as
    /// [`serve`](Self::serve) is. The caller has checked the address (see [`is_loopback`]) and
    /// obtained the token.
    pub async fn serve_http(
        self,
        listener: TcpListener,
        config: HttpConfig,
        mut stop: impl AsyncFnMut(),
    ) -> Result<(), ServeError> {
        let local = listener.local_addr().map_err(ServeError::Listener)?;
        let layer = Arc::clone(self.layer());
        let gone = self.gone().clone();
        let deadline = self.shutdown_deadline();
        let calls = self.calls();
        let sweeper = tokio::spawn(sweep_idle_transactions(
            Arc::clone(&layer),
            self.sweep_interval(),
        ));

        let mut manager = LocalSessionManager::default();
        manager.session_config.keep_alive = Some(config.idle_timeout);
        let sessions = Arc::new(manager);
        let handler = self.with_transport(Transport::Http);
        let rmcp_config = StreamableHttpServerConfig::default()
            .with_allowed_hosts(allowed_hosts(&local, &config.extra_hosts))
            .with_allowed_origins(allowed_origins(&config.extra_origins))
            .with_max_request_body_bytes(config.max_body_bytes)
            .with_cancellation_token(gone.child_token());
        let state = Arc::new(State {
            token: config.token,
            service: StreamableHttpService::new(
                move || Ok(handler.for_session()),
                Arc::clone(&sessions),
                rmcp_config,
            ),
            sessions,
            admission: tokio::sync::Mutex::new(()),
            max_body_bytes: config.max_body_bytes,
            max_sessions: config.max_sessions,
            body_timeout: config.body_timeout,
        });

        let slots = Arc::new(tokio::sync::Semaphore::new(config.max_connections));
        let mut connections = JoinSet::new();
        let signalled = loop {
            tokio::select! {
                accepted = listener.accept() => match accepted {
                    Ok((stream, _)) => {
                        // Over the cap the connection is closed at once: no queue to fill.
                        let Ok(slot) = Arc::clone(&slots).try_acquire_owned() else { continue };
                        let state = Arc::clone(&state);
                        let ended = gone.clone();
                        let header_timeout = config.header_timeout;
                        connections.spawn(async move {
                            let _slot = slot;
                            let served = http1::Builder::new()
                                .timer(TokioTimer::new())
                                .header_read_timeout(header_timeout)
                                .serve_connection(
                                    TokioIo::new(stream),
                                    service_fn(move |request| handle(Arc::clone(&state), request)),
                                );
                            tokio::select! {
                                _ = served => {}
                                () = ended.cancelled() => {}
                            }
                        });
                    }
                    // A reset before the accept, or no file descriptor left: not the end of the
                    // server. Back off briefly so a persistent failure does not spin.
                    Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
                },
                Some(_) = connections.join_next(), if !connections.is_empty() => {}
                () = stop() => break true,
            }
        };
        gone.cancel();
        sweeper.abort();
        connections.abort_all();
        let closed_cleanly = close_down(layer, calls, deadline, &mut stop, signalled).await;
        if closed_cleanly {
            Ok(())
        } else {
            Err(ServeError::TeardownIncomplete)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listening_on_every_interface_says_which_hosts_are_accepted() {
        for addr in ["0.0.0.0:7717", "[::]:7717"] {
            let local: SocketAddr = addr.parse().unwrap();
            let note = wildcard_hosts_note(&local, &[], "TABLEX_MCP_ALLOWED_HOSTS").unwrap();
            assert!(note.contains("TABLEX_MCP_ALLOWED_HOSTS") && note.contains("only localhost, 127.0.0.1 and [::1]"), "{note}");
            assert!(!allowed_hosts(&local, &[]).iter().any(|host| host == "0.0.0.0"));
        }
        let local: SocketAddr = "0.0.0.0:7717".parse().unwrap();
        let note = wildcard_hosts_note(&local, &["box.lan".to_owned()], "X").unwrap();
        assert!(note.contains("[::1] and box.lan"), "{note}");
        for addr in ["127.0.0.1:7717", "192.168.1.5:7717"] {
            assert!(wildcard_hosts_note(&addr.parse().unwrap(), &[], "X").is_none(), "{addr}");
        }
    }
}
