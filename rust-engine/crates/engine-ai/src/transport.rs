//! The HTTP seam. This crate has no HTTP client: the app implements [`HttpTransport`] with its own
//! client, tests implement it with scripted responses.

use crate::key::ApiKey;
use engine_agent::CancelToken;
use std::fmt;
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

/// Sends one request and returns the response head with a streaming body.
///
/// Implementations must honour `cancel` while connecting **and while the body is read**: once the
/// token is cancelled a blocked `send` returns [`TransportError::Cancelled`] and a blocked
/// `Read::read` of the body returns an error (register a hook with [`CancelToken::on_cancel`] that
/// aborts the connection). They must not follow redirects to another host with the credential
/// headers, and must not log the request.
pub trait HttpTransport: Send + Sync {
    /// An `Err` means no byte of the response was received. A non-2xx status is **not** an error
    /// here: it comes back as an [`HttpResponse`].
    fn send(
        &self,
        request: HttpRequest,
        cancel: &CancelToken,
    ) -> Result<HttpResponse, TransportError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
        }
    }
}

/// A header value. A secret one prints as `***` in `Debug` and is read only by [`expose`]
/// (below), which is meant for [`HttpTransport`] implementations when they write the header.
///
/// [`expose`]: HeaderValue::expose
#[derive(Clone)]
pub enum HeaderValue {
    Plain(String),
    Secret(ApiKey),
}

impl HeaderValue {
    pub fn plain(value: impl Into<String>) -> Self {
        Self::Plain(value.into())
    }

    pub fn secret(value: ApiKey) -> Self {
        Self::Secret(value)
    }

    pub fn is_secret(&self) -> bool {
        matches!(self, Self::Secret(_))
    }

    /// The value to put on the wire. **Only [`HttpTransport`] implementations call this**, at the
    /// moment they write the header; do not store, log or format the result.
    pub fn expose(&self) -> &str {
        match self {
            Self::Plain(value) => value,
            Self::Secret(secret) => secret.expose(),
        }
    }
}

impl fmt::Debug for HeaderValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Plain(value) => fmt::Debug::fmt(value, f),
            Self::Secret(_) => f.write_str("***"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Header {
    pub name: &'static str,
    pub value: HeaderValue,
}

impl Header {
    pub fn plain(name: &'static str, value: impl Into<String>) -> Self {
        Self {
            name,
            value: HeaderValue::plain(value),
        }
    }

    pub fn secret(name: &'static str, value: ApiKey) -> Self {
        Self {
            name,
            value: HeaderValue::secret(value),
        }
    }
}

/// Limits for one request; the transport applies them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    /// Opening the connection.
    pub connect: Duration,
    /// From the end of the request to the first byte of the response.
    pub first_byte: Duration,
    /// Between two reads of the body (the stream sends `ping` events, so silence means trouble).
    pub idle: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(15),
            first_byte: Duration::from_secs(120),
            idle: Duration::from_secs(120),
        }
    }
}

pub struct HttpRequest {
    pub method: Method,
    pub url: String,
    pub headers: Vec<Header>,
    /// Shared, so a retry sends the same bytes without copying them.
    pub body: Arc<[u8]>,
    pub timeouts: Timeouts,
}

impl HttpRequest {
    /// The URL as it may be shown: no `user:password@` and no query or fragment.
    pub fn display_url(&self) -> String {
        let url = self.url.split(['?', '#']).next().unwrap_or("");
        match url.split_once("://") {
            Some((scheme, rest)) => {
                let authority_end = rest.find('/').unwrap_or(rest.len());
                match rest[..authority_end].rfind('@') {
                    Some(at) => format!("{scheme}://{}", &rest[at + 1..]),
                    None => url.to_owned(),
                }
            }
            None => url.to_owned(),
        }
    }
}

/// Shows the shape of the request, never the body (it holds the conversation), a secret, the
/// userinfo of the URL or its query.
impl fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpRequest")
            .field("method", &self.method)
            .field("url", &self.display_url())
            .field("headers", &self.headers)
            .field("body_bytes", &self.body.len())
            .field("timeouts", &self.timeouts)
            .finish()
    }
}

pub struct HttpResponse {
    pub status: u16,
    /// Header names as received; look them up with [`header`](Self::header).
    pub headers: Vec<(String, String)>,
    pub body: Box<dyn Read + Send>,
}

impl HttpResponse {
    /// The first header with this name, compared without regard to case.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

impl fmt::Debug for HttpResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpResponse")
            .field("status", &self.status)
            .field("headers", &self.headers)
            .finish_non_exhaustive()
    }
}

/// The request did not produce a response.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TransportError {
    #[error("the request was cancelled")]
    Cancelled,
    #[error("the request timed out")]
    Timeout,
    #[error("the connection failed: {0}")]
    Failed(String),
}
