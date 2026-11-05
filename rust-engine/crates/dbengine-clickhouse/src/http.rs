//! The ClickHouse HTTP(S) interface over a small synchronous client (`ureq` with
//! rustls): requests, error mapping, the reply-size cap and the KILL-based
//! cancel. No SQL decisions are made here.
//!
//! * Every request is a `POST` to `/` with the statement as the body, the
//!   result format in `default_format` and credentials in
//!   `X-ClickHouse-User` / `X-ClickHouse-Key` (never in the URL).
//! * Query parameters (`{name:Type}` placeholders) travel as a
//!   `multipart/form-data` body, so their size is not bound by the URL limit.
//! * Two agents (connection pools): one for user statements, which may run for
//!   hours and have no overall timeout, and one for catalog calls and `KILL`,
//!   which are bounded, so neither ever waits behind a user statement.

use crate::config::Config;
use crate::sql::string_literal;
use arrow_array::RecordBatch;
use arrow_ipc::reader::StreamReader;
use arrow_schema::SchemaRef;
use engine_driver_kit::BackendError;
use serde_json::Value;
use std::fmt;
use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use ureq::tls::{Certificate, RootCerts, TlsConfig};
use ureq::{Agent, Body};

/// The most one reply may weigh (bytes read off the wire) before the query is
/// stopped with `query.reply_too_large`.
pub(crate) const MAX_REPLY_BYTES: u64 = 512 * 1024 * 1024;
/// Catalog replies (system tables) are read into memory up to this size.
const MAX_TEXT_BYTES: u64 = 32 * 1024 * 1024;
const MAX_ERROR_BYTES: u64 = 64 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const META_TIMEOUT: Duration = Duration::from_secs(30);
/// `KILL QUERY`, connect and overall: a cancel inside `disconnect` must not wait
/// the catalog's 15-30 s on a black-holed network (PostgreSQL's and MySQL's cancel use 5 s too).
const KILL_TIMEOUT: Duration = Duration::from_secs(5);
/// Below the server's 30 s keep-alive, so a pooled connection is never one the
/// server already closed.
const IDLE_AGE: Duration = Duration::from_secs(20);
const READ_BUFFER: usize = 256 * 1024;

/// ClickHouse error codes the driver reacts to.
pub(crate) mod code {
    pub const READONLY: u32 = 164;
    pub const QUERY_WAS_CANCELLED: u32 = 394;
    pub const UNKNOWN_DATABASE: u32 = 81;
    /// A type or function the server's Arrow writer does not implement.
    pub const NOT_IMPLEMENTED: u32 = 48;
    pub const UNKNOWN_SETTING: u32 = 115;
    /// Another request of the same HTTP session is still running.
    pub const SESSION_IS_LOCKED: u32 = 373;
    /// The wait for a mutation ran out of time (the mutation itself goes on).
    pub const TIMEOUT_EXCEEDED: u32 = 159;
    /// The `ALTER` was accepted and the mutation failed while it ran (`UNFINISHED`):
    /// it stays queued and is retried by the server.
    pub const UNFINISHED: u32 = 341;
}

/// Which connection pool a request uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Plane {
    /// User statements: no overall timeout.
    #[default]
    Query,
    /// Catalog calls and `DESCRIBE`: bounded.
    Meta,
    /// `KILL QUERY`: bounded tighter than [`Plane::Meta`] ([`KILL_TIMEOUT`]).
    Kill,
}

/// One HTTP request.
#[derive(Default)]
pub(crate) struct Request<'a> {
    pub sql: &'a str,
    /// Rows following the statement in the body (`INSERT ... FORMAT JSONEachRow`).
    pub data: &'a str,
    pub format: Option<&'a str>,
    pub query_id: Option<&'a str>,
    pub session_id: Option<&'a str>,
    pub database: Option<&'a str>,
    /// `(name, escaped value)` for `{name:Type}` placeholders.
    pub params: &'a [(String, String)],
    /// Settings the driver itself needs; they win over the URI's.
    pub settings: &'a [(&'a str, String)],
    pub plane: Plane,
}

#[derive(Debug)]
pub(crate) enum HttpError {
    /// The server answered with an error.
    Server { code: Option<u32>, message: String },
    /// The server could not be reached (DNS, connect, TLS, timeout).
    Connect(String),
    /// The reply broke while it was being read.
    Stream(String),
    /// The reply exceeded [`MAX_REPLY_BYTES`].
    TooLarge,
    /// The reply is not what the driver asked for.
    Protocol(String),
    /// The Arrow reply holds a column the driver reads as the server's own text
    /// (a `Variant`): the statement must run again with that column converted.
    NeedsText,
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Server { message, .. } => f.write_str(message),
            Self::Connect(reason) => write!(f, "cannot reach ClickHouse: {reason}"),
            Self::Stream(reason) => write!(f, "the ClickHouse reply was interrupted: {reason}"),
            Self::TooLarge => write!(
                f,
                "the reply is larger than {} MiB; narrow the query or lower the page size",
                MAX_REPLY_BYTES / (1024 * 1024)
            ),
            Self::Protocol(reason) => f.write_str(reason),
            Self::NeedsText => {
                f.write_str("the result holds a column the Arrow format cannot carry")
            }
        }
    }
}

impl std::error::Error for HttpError {}

impl HttpError {
    pub(crate) fn server_code(&self) -> Option<u32> {
        match self {
            Self::Server { code, .. } => *code,
            _ => None,
        }
    }

    /// The Arrow writer cannot express a column of the result (`AggregateFunction`,
    /// `JSON`, `Dynamic`, `Nothing` ...): the statement can still run with those
    /// columns as the server's text.
    pub(crate) fn is_unsupported_arrow_type(&self) -> bool {
        matches!(self, Self::NeedsText)
            || matches!(self, Self::Server { code: Some(code::NOT_IMPLEMENTED), message } if message.contains("Arrow"))
    }

    /// After a write request failed this way, the server may or may not have
    /// applied it: the connection broke, or the wait was cut short (a cancel,
    /// a timeout) after the statement was accepted. A server's own refusal
    /// (syntax, type, value) is the opposite: nothing was applied.
    pub(crate) fn outcome_unknown(&self) -> bool {
        match self {
            Self::Connect(_) | Self::Stream(_) | Self::TooLarge | Self::NeedsText => true,
            Self::Server { code, .. } => {
                matches!(
                    code,
                    Some(code::QUERY_WAS_CANCELLED | code::TIMEOUT_EXCEEDED | code::UNFINISHED)
                )
            }
            Self::Protocol(_) => false,
        }
    }

    /// The server does not know `setting` (an older version).
    pub(crate) fn is_unknown_setting(&self, setting: &str) -> bool {
        matches!(self, Self::Server { code: Some(code::UNKNOWN_SETTING), message } if message.contains(setting))
    }

    /// The server refused to change a setting in a `readonly` profile.
    pub(crate) fn is_readonly_setting(&self, setting: &str) -> bool {
        matches!(self, Self::Server { code: Some(code::READONLY), message } if message.contains(setting))
    }
}

/// The API area of a call, for the error codes the contract names per area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Area {
    Query,
    Mutation,
    Navigator,
    Describe,
}

impl Area {
    fn failed(self) -> &'static str {
        match self {
            Self::Query => "query.failed",
            Self::Mutation => "mutation.failed",
            Self::Navigator => "navigator.failed",
            Self::Describe => "describe.failed",
        }
    }

    fn cancelled(self) -> Option<&'static str> {
        match self {
            Self::Query => Some("query.cancelled"),
            Self::Mutation => Some("mutation.cancelled"),
            _ => None,
        }
    }

    fn interrupted(self) -> Option<&'static str> {
        match self {
            Self::Query => Some("query.interrupted"),
            Self::Mutation => Some("mutation.interrupted"),
            _ => None,
        }
    }

    pub(crate) fn not_connected(self) -> &'static str {
        match self {
            Self::Query => "query.not_connected",
            Self::Mutation => "mutation.not_connected",
            Self::Navigator => "navigator.not_connected",
            Self::Describe => "describe.not_connected",
        }
    }
}

impl HttpError {
    /// Maps the failure to a wire error. `cancel_requested`: the host asked to
    /// cancel the operation, so an interrupted statement is `<area>.cancelled`;
    /// an interruption nobody asked for is retryable `<area>.interrupted`.
    pub(crate) fn into_backend(self, area: Area, cancel_requested: bool) -> BackendError {
        let interrupted = match &self {
            Self::Server { code, .. } => *code == Some(code::QUERY_WAS_CANCELLED),
            Self::Stream(_) => true,
            _ => false,
        };
        if interrupted {
            if let (Some(cancelled), Some(interrupted_code)) =
                (area.cancelled(), area.interrupted())
            {
                return if cancel_requested {
                    BackendError::with_code(cancelled, self.to_string())
                } else {
                    BackendError::with_code(interrupted_code, self.to_string()).retryable()
                };
            }
        }
        match self {
            Self::TooLarge if area == Area::Query => {
                BackendError::with_code("query.reply_too_large", Self::TooLarge.to_string())
            }
            error @ Self::Connect(_) => {
                BackendError::with_code(area.failed(), error.to_string()).retryable()
            }
            error => BackendError::with_code(area.failed(), error.to_string()),
        }
    }
}

/// The connection to one ClickHouse server: configuration and two pools.
pub(crate) struct Client {
    query_agent: Agent,
    meta_agent: Agent,
    kill_agent: Agent,
    base: String,
    user: String,
    password: Option<String>,
    /// The URI's default database.
    pub(crate) database: Option<String>,
    settings: Vec<(String, String)>,
    /// A `readonly` profile refused `max_result_rows`: stop sending it.
    result_cap_refused: AtomicBool,
    /// The server does not know the optional Arrow output settings: stop sending them.
    arrow_extras_refused: AtomicBool,
    /// The server does not know `mutations_execute_subqueries_on_initiator`.
    initiator_subqueries_refused: AtomicBool,
}

fn agent(
    tls: Option<&TlsConfig>,
    connect_timeout: Duration,
    global_timeout: Option<Duration>,
) -> Agent {
    let mut builder = Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_connect(Some(connect_timeout))
        .timeout_global(global_timeout)
        .max_idle_age(IDLE_AGE)
        .user_agent("tablex-clickhouse-driver");
    if let Some(tls) = tls {
        builder = builder.tls_config(tls.clone());
    }
    Agent::new_with_config(builder.build())
}

/// Every certificate of a PEM bundle, or the single certificate of a DER file.
/// The pool of `KILL QUERY`: [`KILL_TIMEOUT`] to connect and for the whole call.
fn kill_agent(tls: Option<&TlsConfig>) -> Agent {
    agent(tls, KILL_TIMEOUT, Some(KILL_TIMEOUT))
}

fn certificates(bytes: &[u8]) -> Result<Vec<Certificate<'static>>, String> {
    const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
    const END: &str = "-----END CERTIFICATE-----";
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Ok(vec![Certificate::from_der(bytes).to_owned()]);
    };
    let mut certificates = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(BEGIN) {
        let block = &rest[start..];
        let end = block
            .find(END)
            .ok_or("a certificate block has no END line")?
            + END.len();
        certificates.push(
            Certificate::from_pem(&block.as_bytes()[..end]).map_err(|error| error.to_string())?,
        );
        rest = &block[end..];
    }
    if certificates.is_empty() {
        return Err("the file holds no certificate".into());
    }
    Ok(certificates)
}

fn tls_config(ca_cert: &Path) -> Result<TlsConfig, BackendError> {
    let bad = |reason: String| {
        BackendError::with_code(
            "connect.bad_request",
            format!("CA certificate {}: {reason}", ca_cert.display()),
        )
    };
    let bytes = std::fs::read(ca_cert).map_err(|error| bad(error.to_string()))?;
    let certificates = certificates(&bytes).map_err(bad)?;
    Ok(TlsConfig::builder()
        .root_certs(RootCerts::new_with_certs(&certificates))
        .build())
}

impl Client {
    /// Builds the pools and checks the server answers (`SELECT 1`), so bad
    /// credentials or an unknown default database fail the connect.
    pub(crate) fn connect(config: &Config) -> Result<Self, BackendError> {
        let tls = match (&config.ca_cert, config.secure) {
            (Some(path), true) => Some(tls_config(path)?),
            _ => None,
        };
        let client = Self {
            query_agent: agent(tls.as_ref(), CONNECT_TIMEOUT, None),
            meta_agent: agent(tls.as_ref(), CONNECT_TIMEOUT, Some(META_TIMEOUT)),
            kill_agent: kill_agent(tls.as_ref()),
            base: config.base_url(),
            user: config.user.clone(),
            password: config.password.clone(),
            database: config.database.clone(),
            settings: config.settings.clone(),
            result_cap_refused: AtomicBool::new(false),
            arrow_extras_refused: AtomicBool::new(false),
            initiator_subqueries_refused: AtomicBool::new(false),
        };
        client
            .rows_in("SELECT 1", &[], client.database.as_deref())
            .map_err(|error| {
                let message = match &error {
                    HttpError::Server {
                        code: Some(516 | 193),
                        ..
                    } => {
                        format!("authentication failed for user `{}`", client.user)
                    }
                    other => format!("{other} ({})", client.base),
                };
                BackendError::with_code("connect.failed", message)
            })?;
        Ok(client)
    }

    pub(crate) fn result_cap_allowed(&self) -> bool {
        !self.result_cap_refused.load(Ordering::Relaxed)
    }

    pub(crate) fn refuse_result_cap(&self) {
        self.result_cap_refused.store(true, Ordering::Relaxed);
    }

    /// Whether to send `output_format_arrow_date_as_uint16` and
    /// `output_format_arrow_unsupported_types_as_binary`: they pin the Arrow
    /// types the driver converts, but an older server does not know them.
    pub(crate) fn arrow_extras_allowed(&self) -> bool {
        !self.arrow_extras_refused.load(Ordering::Relaxed)
    }

    pub(crate) fn refuse_arrow_extras(&self) {
        self.arrow_extras_refused.store(true, Ordering::Relaxed);
    }

    /// Whether to send `mutations_execute_subqueries_on_initiator` with an
    /// `ALTER` (see `mutation`): an older server does not know it.
    pub(crate) fn initiator_subqueries_allowed(&self) -> bool {
        !self.initiator_subqueries_refused.load(Ordering::Relaxed)
    }

    pub(crate) fn refuse_initiator_subqueries(&self) {
        self.initiator_subqueries_refused
            .store(true, Ordering::Relaxed);
    }

    /// Sends `request` and returns the reply once the status line and headers
    /// are in; the body is still streaming.
    pub(crate) fn send(&self, request: &Request<'_>) -> Result<Reply, HttpError> {
        let agent = match request.plane {
            Plane::Query => &self.query_agent,
            Plane::Meta => &self.meta_agent,
            Plane::Kill => &self.kill_agent,
        };
        let mut builder = agent
            .post(format!("{}/", self.base))
            .header("X-ClickHouse-User", &self.user);
        if let Some(password) = &self.password {
            builder = builder.header("X-ClickHouse-Key", password);
        }
        // User settings first; the driver's own win on a clash.
        let fixed = [
            ("default_format", request.format),
            ("query_id", request.query_id),
            ("session_id", request.session_id),
            ("database", request.database),
        ];
        let mut pairs: Vec<(&str, &str)> = self
            .settings
            .iter()
            .filter(|(name, _)| !request.settings.iter().any(|(own, _)| own == name))
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        pairs.extend(
            request
                .settings
                .iter()
                .map(|(name, value)| (*name, value.as_str())),
        );
        pairs.extend(
            fixed
                .iter()
                .filter_map(|(name, value)| value.map(|value| (*name, value))),
        );
        if request.session_id.is_some() {
            pairs.push(("session_timeout", "3600"));
            pairs.push(("session_check", "0"));
        }
        builder = builder.query_pairs(pairs);
        let response = if request.params.is_empty() {
            let body = if request.data.is_empty() {
                request.sql.to_owned()
            } else {
                format!("{}\n{}", request.sql, request.data)
            };
            builder.send(body)
        } else {
            let (content_type, body) = multipart(request.sql, request.params);
            builder.header("Content-Type", content_type).send(body)
        }
        .map_err(|error| HttpError::Connect(error.to_string()))?;
        let status = response.status().as_u16();
        let exception_code = response
            .headers()
            .get("X-ClickHouse-Exception-Code")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse().ok());
        let format = response
            .headers()
            .get("X-ClickHouse-Format")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let written_rows = response
            .headers()
            .get("X-ClickHouse-Summary")
            .and_then(|value| value.to_str().ok())
            .and_then(written_rows);
        let mut body = response.into_body();
        if status >= 400 {
            let mut text = String::new();
            let read = body
                .as_reader()
                .take(MAX_ERROR_BYTES)
                .read_to_string(&mut text);
            return Err(server_error(status, exception_code, &text, read.err()));
        }
        Ok(Reply {
            body,
            written_rows,
            format,
        })
    }

    /// A catalog query: the rows as JSON values (64-bit integers are numbers).
    pub(crate) fn rows(
        &self,
        sql: &str,
        params: &[(String, String)],
    ) -> Result<Vec<Vec<Value>>, HttpError> {
        self.rows_in(sql, params, None)
    }

    pub(crate) fn rows_in(
        &self,
        sql: &str,
        params: &[(String, String)],
        database: Option<&str>,
    ) -> Result<Vec<Vec<Value>>, HttpError> {
        self.rows_on(Plane::Meta, sql, params, database, None)
    }

    /// A catalog query inside the HTTP session `session`, so temporary tables
    /// and the session's settings apply. The session serves one request at a
    /// time: call it only while the user statement's reply is finished.
    pub(crate) fn rows_in_session(
        &self,
        sql: &str,
        database: Option<&str>,
        session: &str,
    ) -> Result<Vec<Vec<Value>>, HttpError> {
        self.rows_on(Plane::Meta, sql, &[], database, Some(session))
    }

    fn rows_on(
        &self,
        plane: Plane,
        sql: &str,
        params: &[(String, String)],
        database: Option<&str>,
        session: Option<&str>,
    ) -> Result<Vec<Vec<Value>>, HttpError> {
        let reply = self.send(&Request {
            sql,
            format: Some("JSONCompactEachRow"),
            session_id: session,
            database,
            params,
            settings: &[("output_format_json_quote_64bit_integers", "0".into())],
            plane,
            ..Request::default()
        })?;
        reply
            .text()?
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| {
                serde_json::from_str(line).map_err(|error| {
                    HttpError::Protocol(format!("unreadable catalog row: {error}"))
                })
            })
            .collect()
    }

    /// Stops the running query `query_id`; the number of queries the server found.
    pub(crate) fn kill_query(&self, query_id: &str) -> Result<usize, HttpError> {
        let sql = format!(
            "KILL QUERY WHERE query_id = {} FORMAT JSONCompactEachRow",
            string_literal(query_id)
        );
        Ok(self.rows_on(Plane::Kill, &sql, &[], None, None)?.len())
    }
}

/// `multipart/form-data` with the statement as field `query` and one
/// `param_<name>` field per parameter.
fn multipart(sql: &str, params: &[(String, String)]) -> (String, Vec<u8>) {
    use std::hash::{BuildHasher, Hasher};
    let nonce = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    let boundary = format!("tablex-{nonce:016x}");
    let mut body = Vec::with_capacity(sql.len() + 128 * (params.len() + 1));
    let mut field = |name: &str, value: &str| {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n")
                .as_bytes(),
        );
        body.extend_from_slice(value.as_bytes());
        body.extend_from_slice(b"\r\n");
    };
    field("query", sql);
    for (name, value) in params {
        field(&format!("param_{name}"), value);
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

/// `written_rows` of the `X-ClickHouse-Summary` header.
fn written_rows(summary: &str) -> Option<u64> {
    serde_json::from_str::<Value>(summary)
        .ok()?
        .get("written_rows")
        .and_then(|rows| {
            rows.as_str()
                .and_then(|text| text.parse().ok())
                .or_else(|| rows.as_u64())
        })
}

fn server_error(
    status: u16,
    header_code: Option<u32>,
    text: &str,
    read: Option<io::Error>,
) -> HttpError {
    let text = text.trim();
    let from_text = text
        .strip_prefix("Code: ")
        .and_then(|rest| rest.split('.').next())
        .and_then(|digits| digits.trim().parse().ok());
    // The version banner at the end is noise in a dialog.
    let message = text
        .rfind(" (version ")
        .map_or(text, |end| &text[..end])
        .trim();
    let message = match (message.is_empty(), read) {
        (false, _) => message.to_owned(),
        (true, Some(error)) => format!("HTTP {status}: {error}"),
        (true, None) => format!("HTTP {status}"),
    };
    HttpError::Server {
        code: header_code.or(from_text),
        message,
    }
}

/// A reply whose headers arrived.
pub(crate) struct Reply {
    body: Body,
    /// Rows the statement wrote, from `X-ClickHouse-Summary` (inserts, DDL).
    pub written_rows: Option<u64>,
    /// `X-ClickHouse-Format`: the format the server answered in.
    pub format: Option<String>,
}

impl Reply {
    /// The whole body as text (catalog replies).
    pub(crate) fn text(mut self) -> Result<String, HttpError> {
        let mut text = String::new();
        self.body
            .as_reader()
            .take(MAX_TEXT_BYTES)
            .read_to_string(&mut text)
            .map_err(|error| HttpError::Stream(error.to_string()))?;
        Ok(text)
    }

    /// Reads the body as an Arrow IPC stream. `None` when the statement
    /// produced no result set (an empty body: DDL, `INSERT`).
    pub(crate) fn arrow(self) -> Result<Option<ArrowBody>, HttpError> {
        if let Some(format) = self
            .format
            .as_deref()
            .filter(|format| *format != "ArrowStream")
        {
            return Err(HttpError::Protocol(format!(
                "the server answered in {format}; the driver reads results as ArrowStream, \
                 so remove the FORMAT clause from the statement"
            )));
        }
        let exceeded = Arc::new(AtomicBool::new(false));
        let counted = Counted {
            inner: self.body.into_reader(),
            remaining: MAX_REPLY_BYTES,
            exceeded: Arc::clone(&exceeded),
        };
        let mut buffered = BufReader::with_capacity(READ_BUFFER, counted);
        let stream_error = |error: &dyn fmt::Display, exceeded: &AtomicBool| {
            if exceeded.load(Ordering::Relaxed) {
                HttpError::TooLarge
            } else {
                HttpError::Stream(error.to_string())
            }
        };
        if buffered
            .fill_buf()
            .map_err(|error| stream_error(&error, &exceeded))?
            .is_empty()
        {
            return Ok(None);
        }
        let reader = StreamReader::try_new(buffered, None)
            .map_err(|error| stream_error(&error, &exceeded))?;
        Ok(Some(ArrowBody { reader, exceeded }))
    }
}

/// Caps the bytes read off the wire.
struct Counted<R> {
    inner: R,
    remaining: u64,
    exceeded: Arc<AtomicBool>,
}

impl<R: Read> Read for Counted<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 {
            self.exceeded.store(true, Ordering::Relaxed);
            return Err(io::Error::other("reply too large"));
        }
        let room =
            usize::try_from(self.remaining).map_or(buf.len(), |remaining| remaining.min(buf.len()));
        let read = self.inner.read(&mut buf[..room])?;
        self.remaining -= read as u64;
        Ok(read)
    }
}

type ArrowReader = StreamReader<BufReader<Counted<ureq::BodyReader<'static>>>>;

/// An Arrow IPC stream being read from the wire, one record batch at a time.
pub(crate) struct ArrowBody {
    reader: ArrowReader,
    exceeded: Arc<AtomicBool>,
}

impl ArrowBody {
    pub(crate) fn schema(&self) -> SchemaRef {
        self.reader.schema()
    }

    pub(crate) fn next_batch(&mut self) -> Result<Option<RecordBatch>, HttpError> {
        match self.reader.next() {
            None => Ok(None),
            Some(Ok(batch)) => Ok(Some(batch)),
            Some(Err(error)) if self.exceeded.load(Ordering::Relaxed) => {
                let _ = error;
                Err(HttpError::TooLarge)
            }
            Some(Err(error)) => Err(HttpError::Stream(error.to_string())),
        }
    }

    /// After the last batch: reads the HTTP trailer so the connection returns to
    /// the pool. Dropping an unfinished body closes the connection instead.
    pub(crate) fn finish(mut self) {
        let inner = self.reader.get_mut();
        let _ = io::copy(&mut inner.by_ref().take(1 << 20), &mut io::sink());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client_with_kill_agent(kill_agent: Agent) -> Client {
        Client {
            query_agent: agent(None, CONNECT_TIMEOUT, None),
            meta_agent: agent(None, CONNECT_TIMEOUT, Some(META_TIMEOUT)),
            kill_agent,
            base: String::new(),
            user: "default".into(),
            password: None,
            database: None,
            settings: Vec::new(),
            result_cap_refused: AtomicBool::new(false),
            arrow_extras_refused: AtomicBool::new(false),
            initiator_subqueries_refused: AtomicBool::new(false),
        }
    }

    #[test]
    fn the_kill_has_its_own_short_timeouts() {
        let timeouts = kill_agent(None).config().timeouts();
        assert_eq!(timeouts.connect, Some(KILL_TIMEOUT));
        assert_eq!(timeouts.global, Some(KILL_TIMEOUT));
        assert!(KILL_TIMEOUT <= Duration::from_secs(5));
        let meta = agent(None, CONNECT_TIMEOUT, Some(META_TIMEOUT));
        assert!(timeouts.global < meta.config().timeouts().global);
    }

    #[test]
    fn a_kill_to_a_server_that_never_answers_ends_at_its_own_timeout() {
        // Accepts (the kernel does, through the backlog) and never reads or replies.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let short = Duration::from_millis(300);
        let mut client = client_with_kill_agent(agent(None, short, Some(short)));
        client.base = format!("http://{}", listener.local_addr().unwrap());
        let started = std::time::Instant::now();
        let result = client.kill_query("tablex-1-1-1");
        assert!(matches!(result, Err(HttpError::Connect(_))), "{result:?}");
        // The catalog's 30 s would have been waited out here; allow a loaded runner.
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn server_errors_carry_the_code_without_the_version_banner() {
        let error = server_error(
            500,
            None,
            "Code: 62. DB::Exception: Syntax error: failed at position 1. (SYNTAX_ERROR) (version 26.9.12.8 (official build))\n",
            None,
        );
        let HttpError::Server { code, message } = error else {
            panic!()
        };
        assert_eq!(code, Some(62));
        assert!(message.ends_with("(SYNTAX_ERROR)"), "{message}");
        // The header wins; an empty body still names the status.
        let HttpError::Server { code, message } = server_error(403, Some(516), "", None) else {
            panic!()
        };
        assert_eq!((code, message.as_str()), (Some(516), "HTTP 403"));
    }

    #[test]
    fn the_refusals_the_driver_answers_are_told_apart() {
        let server = |code: u32, message: &str| HttpError::Server {
            code: Some(code),
            message: message.into(),
        };
        let unsupported = server(
            code::NOT_IMPLEMENTED,
            "Native Arrow IPC writer does not support type AggregateFunction(sum, UInt64)",
        );
        assert!(unsupported.is_unsupported_arrow_type());
        assert!(server(
            code::NOT_IMPLEMENTED,
            "Method getDataAt is not supported for Object(max_dynamic_paths=1024): While executing ArrowIPCBlockOutputFormat."
        )
        .is_unsupported_arrow_type());
        assert!(HttpError::NeedsText.is_unsupported_arrow_type());
        assert!(
            !server(code::NOT_IMPLEMENTED, "Window functions are not supported")
                .is_unsupported_arrow_type()
        );
        assert!(!server(62, "Arrow syntax error").is_unsupported_arrow_type());
        let setting = "output_format_arrow_date_as_uint16";
        assert!(server(
            code::UNKNOWN_SETTING,
            &format!("Setting {setting} is neither a builtin setting")
        )
        .is_unknown_setting(setting));
        assert!(
            !server(code::UNKNOWN_SETTING, "Setting other is neither").is_unknown_setting(setting)
        );
        assert!(!server(62, setting).is_unknown_setting(setting));
    }

    #[test]
    fn a_write_that_broke_midway_has_an_unknown_outcome() {
        let server = |code: u32| HttpError::Server {
            code: Some(code),
            message: "x".into(),
        };
        for unknown in [
            HttpError::Connect("reset".into()),
            HttpError::Stream("eof".into()),
            server(code::QUERY_WAS_CANCELLED),
            server(code::TIMEOUT_EXCEEDED),
            // Accepted, then failed while running: still queued on the server.
            server(code::UNFINISHED),
        ] {
            assert!(unknown.outcome_unknown(), "{unknown:?}");
        }
        // The server's own refusals changed nothing.
        for refused in [server(62), server(131), server(code::READONLY)] {
            assert!(!refused.outcome_unknown(), "{refused:?}");
        }
        assert!(!HttpError::Protocol("x".into()).outcome_unknown());
    }

    #[test]
    fn written_rows_come_from_the_summary_header() {
        assert_eq!(
            written_rows(r#"{"read_rows":"3","written_rows":"3"}"#),
            Some(3)
        );
        assert_eq!(written_rows(r#"{"written_rows":7}"#), Some(7));
        assert_eq!(written_rows("garbage"), None);
    }

    #[test]
    fn multipart_carries_the_statement_and_every_parameter() {
        let (content_type, body) = multipart(
            "SELECT {a:String}",
            &[("a".into(), "x\\ty".into()), ("b".into(), "\\N".into())],
        );
        let boundary = content_type
            .strip_prefix("multipart/form-data; boundary=")
            .unwrap();
        let body = String::from_utf8(body).unwrap();
        assert!(body.contains("name=\"query\"\r\n\r\nSELECT {a:String}\r\n"));
        assert!(body.contains("name=\"param_a\"\r\n\r\nx\\ty\r\n"));
        assert!(body.contains("name=\"param_b\"\r\n\r\n\\N\r\n"));
        assert!(body.ends_with(&format!("--{boundary}--\r\n")));
    }

    #[test]
    fn errors_map_to_the_contract_codes() {
        let cancelled = || HttpError::Server {
            code: Some(code::QUERY_WAS_CANCELLED),
            message: "killed".into(),
        };
        let requested = cancelled().into_backend(Area::Query, true);
        assert_eq!(requested.code.as_deref(), Some("query.cancelled"));
        assert!(!requested.retryable);
        let unrequested = cancelled().into_backend(Area::Query, false);
        assert_eq!(unrequested.code.as_deref(), Some("query.interrupted"));
        assert!(unrequested.retryable);
        let mutation = HttpError::Stream("eof".into()).into_backend(Area::Mutation, true);
        assert_eq!(mutation.code.as_deref(), Some("mutation.cancelled"));
        let large = HttpError::TooLarge.into_backend(Area::Query, false);
        assert_eq!(large.code.as_deref(), Some("query.reply_too_large"));
        let unreachable = HttpError::Connect("refused".into()).into_backend(Area::Navigator, false);
        assert_eq!(unreachable.code.as_deref(), Some("navigator.failed"));
        assert!(unreachable.retryable);
        let plain = HttpError::Server {
            code: Some(62),
            message: "syntax".into(),
        }
        .into_backend(Area::Query, false);
        assert_eq!(
            (plain.code.as_deref(), plain.retryable),
            (Some("query.failed"), false)
        );
    }

    #[test]
    fn pem_bundles_and_garbage_are_told_apart() {
        assert!(certificates(b"no certificate here")
            .unwrap_err()
            .contains("no certificate"));
        assert!(certificates(b"-----BEGIN CERTIFICATE-----\nAAAA").is_err());
    }

    #[test]
    fn the_reply_cap_stops_reads() {
        let exceeded = Arc::new(AtomicBool::new(false));
        let mut counted = Counted {
            inner: io::Cursor::new(vec![7_u8; 10]),
            remaining: 4,
            exceeded: Arc::clone(&exceeded),
        };
        let mut buf = [0_u8; 8];
        assert_eq!(counted.read(&mut buf).unwrap(), 4);
        assert!(counted.read(&mut buf).is_err());
        assert!(exceeded.load(Ordering::Relaxed));
    }
}
