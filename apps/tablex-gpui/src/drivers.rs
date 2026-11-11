//! Driver downloads, the catalog fetcher and update checks. The store directory, the manifest trust
//! keys and policy and verified loading are shared with `tablex-mcp` and live in
//! `engine-connect::drivers`; they are re-exported here under the old path.

use engine_driver_host::{
    classify_download_url, stream_to_file, DownloadError, DownloadTransport, DownloadedArtifact,
    InstalledDriverStore, ProgressSink, TrustPolicy,
};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

pub use engine_connect::drivers::*;

const MAX_MANIFEST_BYTES: u64 = 2 * 1024 * 1024;
static DOWNLOAD_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// Minimum time between two background manifest checks.
pub const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

pub fn catalog_generation_is_current(current: u64, result: u64) -> bool {
    current == result
}

/// A catalog fetch failure as the user should read it: a local manifest without a signature in
/// strict mode is a policy matter (Developer mode accepts it), not a corrupt file. Any other
/// failure, a bad signature included, is reported as it is: a correctly signed local copy is
/// accepted in strict mode.
pub fn explain_registry_error(
    error: &engine_driver_host::RegistryError,
    source: &str,
    policy: TrustPolicy,
) -> String {
    let local = url::Url::parse(source).is_ok_and(|url| url.scheme() == "file");
    match error {
        engine_driver_host::RegistryError::Manifest(
            engine_driver_host::ManifestTrustError::SignatureMissing,
        ) if local && policy == TrustPolicy::Strict => {
            format!(
                "Unsigned local driver manifests are only accepted in Developer mode (\u{22EF} menu): {error}"
            )
        }
        _ => error.to_string(),
    }
}

/// Where unfinished downloads live (inside the driver store, same volume as
/// the install target). "Clear download cache" empties it.
pub fn downloads_dir(store: &InstalledDriverStore) -> PathBuf {
    store.base_directory().join("Drivers").join("v2").join("downloads")
}

/// A fresh, not yet existing download path (`stream_to_file` creates it
/// exclusively); a `.zip` URL keeps its extension so the installer unpacks it.
pub fn temporary_download_path(store: &InstalledDriverStore, id: &str, source: &str) -> PathBuf {
    let extension = url::Url::parse(source)
        .ok()
        .and_then(|url| {
            Path::new(url.path())
                .extension()
                .and_then(|extension| extension.to_str())
                .map(str::to_owned)
        })
        .filter(|extension| extension.eq_ignore_ascii_case("zip"))
        .map(|_| "zip")
        .unwrap_or("artifact");
    downloads_dir(store).join(format!(
        "{}-{}-{id}.{extension}",
        std::process::id(),
        DOWNLOAD_SEQUENCE.fetch_add(1, Ordering::Relaxed),
    ))
}

fn user_agent() -> String {
    format!("TableX/{} ({})", env!("CARGO_PKG_VERSION"), current_target().id())
}

/// How long a download or a manifest fetch may receive nothing before it fails (a stalled server
/// must not hold the Driver Manager, nor a cancel, for the length of the request timeout).
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// The blocking client's request timeout for an artifact. It is applied to each wait (connecting,
/// the head, every read of the body), NOT to the download as a whole: a slow but steady download
/// of any length is fine, and a read that blocks is cut off by this at the latest.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Downloads `url` into `destination` under `policy` (HTTPS only; `file://`
/// only for developers), hashing as it streams. Progress goes to `sink`.
pub fn download_artifact(
    url: &str,
    destination: &Path,
    expected_size: u64,
    policy: TrustPolicy,
    cancelled: &Arc<AtomicBool>,
    sink: &dyn ProgressSink,
) -> Result<DownloadedArtifact, DownloadError> {
    let transport = classify_download_url(url, policy)?;
    if let Some(directory) = destination.parent() {
        std::fs::create_dir_all(directory)?;
    }
    let mut input = open_artifact(url, expected_size, transport, policy, cancelled)?;
    stream_to_file(&mut *input, destination, expected_size, cancelled, sink)
}

/// The byte source of an artifact whose `url` already passed
/// [`classify_download_url`] (`transport` is its answer): a local file or
/// the body of an HTTP response no longer than `expected_size`.
///
/// * Redirects: every hop is classified again under `policy`, and a redirect never leaves
///   `https` (see [`redirect_allowed`]).
/// * The whole request (connect, head, body) runs on a helper thread and is waited for in slices
///   of a quarter second: `cancelled` returns within about that long in any phase, and a server
///   that sends nothing for [`IDLE_TIMEOUT`] (no head, or no more body) fails with `TimedOut`
///   (as `DownloadError::Io` / a read error), sooner than the request's own per-wait timeout.
pub fn open_artifact(
    url: &str,
    expected_size: u64,
    transport: DownloadTransport,
    policy: TrustPolicy,
    cancelled: &Arc<AtomicBool>,
) -> Result<Box<dyn Read>, DownloadError> {
    open_artifact_idle(url, expected_size, transport, policy, IDLE_TIMEOUT, cancelled)
}

fn open_artifact_idle(
    url: &str,
    expected_size: u64,
    transport: DownloadTransport,
    policy: TrustPolicy,
    idle: Duration,
    cancelled: &Arc<AtomicBool>,
) -> Result<Box<dyn Read>, DownloadError> {
    match transport {
        DownloadTransport::LocalFile => {
            let path = url::Url::parse(url)
                .ok()
                .and_then(|url| url.to_file_path().ok())
                .ok_or_else(|| DownloadError::UnsupportedScheme(url.to_string()))?;
            Ok(Box::new(std::fs::File::open(path)?))
        }
        DownloadTransport::Http => {
            let (head, reader) = http_stream(url, policy, DOWNLOAD_TIMEOUT, idle, cancelled)?;
            if !head.success {
                return Err(DownloadError::Io(std::io::Error::other(format!(
                    "download failed with HTTP {}",
                    head.status
                ))));
            }
            if head.content_length.is_some_and(|length| length > expected_size) {
                return Err(DownloadError::TooLarge {
                    limit: expected_size,
                });
            }
            Ok(Box::new(reader))
        }
    }
}

/// What the response head says.
struct Head {
    status: String,
    success: bool,
    content_length: Option<u64>,
}

/// What the helper thread of a request sends: the head, chunks of the body, then `End`; or the
/// failure of the request / of a read.
enum Message {
    Head(Head),
    Chunk(Vec<u8>),
    End,
    /// `send()` failed; `refused` when our redirect policy said no.
    HeadFailed { message: String, refused: bool },
    Failed(std::io::Error),
}

/// A redirect refused by [`redirect_allowed`]; found again in the error chain of reqwest's error.
#[derive(Debug)]
struct RedirectRefused(String);

impl std::fmt::Display for RedirectRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RedirectRefused {}

/// An error with its causes: reqwest's own text for a refused redirect is only "error following
/// redirect", the reason is the source. Also whether our redirect policy is among the causes.
fn describe(error: &(dyn std::error::Error + 'static)) -> (String, bool) {
    let mut text = error.to_string();
    let mut refused = error.is::<RedirectRefused>();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        refused |= cause.is::<RedirectRefused>();
        source = cause.source();
    }
    (text, refused)
}

/// May a download that came from `previous` be redirected to `next`? The integrity of a download
/// rests on its size and hash, but where it is fetched from stays under the URL policy: every hop
/// is classified again (https, plain http only to loopback), and a redirect never leaves `https`:
/// a signed manifest names an https host, and that host must not be able to send the app to a
/// plain-http service on this machine. A first URL of plain-http loopback is a different case (it
/// can only come from a signed manifest or the developer policy) and may redirect to http.
fn redirect_allowed(
    previous: Option<&url::Url>,
    next: &url::Url,
    policy: TrustPolicy,
) -> Result<(), String> {
    if previous.is_some_and(|previous| previous.scheme() == "https") && next.scheme() != "https" {
        return Err(format!(
            "refusing a redirect from https to {next}: a redirect never leaves https"
        ));
    }
    match classify_download_url(next.as_str(), policy) {
        Ok(DownloadTransport::Http) => Ok(()),
        _ => Err(format!(
            "refusing a redirect to {next}: downloads require HTTPS"
        )),
    }
}

fn redirect_policy(policy: TrustPolicy) -> zed_reqwest::redirect::Policy {
    zed_reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= 10 {
            return attempt.error("too many redirects");
        }
        // Without a previous hop (not expected) the target is still classified: fail closed.
        match redirect_allowed(attempt.previous().last(), attempt.url(), policy) {
            Ok(()) => attempt.follow(),
            Err(refused) => attempt.error(RedirectRefused(refused)),
        }
    })
}

/// Sends the request on a helper thread and returns the response head and a reader of the body
/// (see [`open_artifact`] for what it guarantees).
fn http_stream(
    url: &str,
    policy: TrustPolicy,
    total_timeout: Duration,
    idle: Duration,
    cancelled: &Arc<AtomicBool>,
) -> Result<(Head, IdleReader), DownloadError> {
    let (sender, messages) = std::sync::mpsc::sync_channel(4);
    let url = url.to_string();
    let spawned = std::thread::Builder::new()
        .name("download-request".into())
        .spawn(move || {
            let sent = zed_reqwest::blocking::Client::builder()
                .user_agent(user_agent())
                .connect_timeout(Duration::from_secs(15))
                .timeout(Some(total_timeout))
                .redirect(redirect_policy(policy))
                .build()
                .and_then(|client| client.get(&url).send());
            match sent {
                Err(error) => {
                    let (message, refused) = describe(&error);
                    let _ = sender.send(Message::HeadFailed { message, refused });
                }
                Ok(response) => {
                    let head = Head {
                        status: response.status().to_string(),
                        success: response.status().is_success(),
                        content_length: response.content_length(),
                    };
                    if sender.send(Message::Head(head)).is_ok() {
                        pump(response, &sender);
                    }
                }
            }
        });
    spawned.map_err(DownloadError::Io)?;
    let mut reader = IdleReader::new(messages, idle, Arc::clone(cancelled));
    match reader.next_message() {
        Ok(Message::Head(head)) => Ok((head, reader)),
        Ok(Message::HeadFailed { message, refused: true }) => {
            Err(DownloadError::InsecureUrl(message))
        }
        Ok(Message::HeadFailed { message, .. }) => {
            Err(DownloadError::Io(std::io::Error::other(message)))
        }
        Ok(_) => Err(DownloadError::Io(std::io::Error::other("unexpected reply"))),
        Err(_) if cancelled.load(Ordering::Acquire) => Err(DownloadError::Cancelled),
        Err(error) => Err(DownloadError::Io(error)),
    }
}

/// Reads `source` chunk by chunk into the channel: `Interrupted` is retried here, end of body is
/// an explicit `End` (so a helper that dies cannot pass for a complete body), any other error is
/// forwarded once.
fn pump(mut source: impl Read, sender: &std::sync::mpsc::SyncSender<Message>) {
    loop {
        let mut chunk = vec![0_u8; 64 * 1024];
        match source.read(&mut chunk) {
            Ok(0) => {
                let _ = sender.send(Message::End);
                return;
            }
            Ok(read) => {
                chunk.truncate(read);
                if sender.send(Message::Chunk(chunk)).is_err() {
                    return;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => {
                let _ = sender.send(Message::Failed(error));
                return;
            }
        }
    }
}

/// How often a wait for the helper thread looks at the cancel flag.
const CANCEL_POLL: Duration = Duration::from_millis(250);

/// The reading end of a helper thread's channel. The helper stays blocked in a read that never
/// returns until the request's per-wait timeout (see [`DOWNLOAD_TIMEOUT`]) or the server closes the
/// connection, then ends; the caller does not wait for it. A dropped reader just
/// makes its next send fail. One 64 KiB buffer per chunk is the extra cost on a download of a few
/// megabytes. A failure is sticky: every later read fails again, none reads as end of file, and
/// end of file is only the helper's explicit `End`.
struct IdleReader {
    messages: std::sync::mpsc::Receiver<Message>,
    current: std::io::Cursor<Vec<u8>>,
    idle: Duration,
    cancelled: Arc<AtomicBool>,
    last_progress: std::time::Instant,
    outcome: Option<Result<(), (std::io::ErrorKind, String)>>,
}

impl IdleReader {
    fn new(
        messages: std::sync::mpsc::Receiver<Message>,
        idle: Duration,
        cancelled: Arc<AtomicBool>,
    ) -> Self {
        Self {
            messages,
            current: std::io::Cursor::new(Vec::new()),
            idle,
            cancelled,
            last_progress: std::time::Instant::now(),
            outcome: None,
        }
    }

    /// A reader of `source` on its own helper thread (the tests of the reader itself).
    #[cfg(test)]
    fn of(source: impl Read + Send + 'static, idle: Duration, cancelled: Arc<AtomicBool>) -> Self {
        let (sender, messages) = std::sync::mpsc::sync_channel(4);
        if let Err(error) = std::thread::Builder::new()
            .name("download-read".into())
            .spawn(move || pump(source, &sender))
        {
            let (sender, failed) = std::sync::mpsc::sync_channel(1);
            let _ = sender.send(Message::Failed(error));
            return Self::new(failed, idle, cancelled);
        }
        Self::new(messages, idle, cancelled)
    }

    /// The next message, waiting at most `idle` since the last one and looking at the cancel flag
    /// every [`CANCEL_POLL`].
    fn next_message(&mut self) -> std::io::Result<Message> {
        loop {
            if self.cancelled.load(Ordering::Acquire) {
                return Err(std::io::Error::other("cancelled"));
            }
            // What is already waiting is delivered first: a consumer that was itself stopped for
            // longer than the idle time is not a stalled server.
            match self.messages.try_recv() {
                Ok(message) => {
                    self.last_progress = std::time::Instant::now();
                    return Ok(message);
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "the download reader stopped before the end of the body",
                    ))
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
            let Some(left) = self.idle.checked_sub(self.last_progress.elapsed()) else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("no data received for {} s", self.idle.as_secs_f32().round()),
                ));
            };
            match self.messages.recv_timeout(left.min(CANCEL_POLL)) {
                Ok(message) => {
                    self.last_progress = std::time::Instant::now();
                    return Ok(message);
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "the download reader stopped before the end of the body",
                    ))
                }
            }
        }
    }
}

impl Read for IdleReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        loop {
            match &self.outcome {
                Some(Err((kind, message))) => return Err(std::io::Error::new(*kind, message.clone())),
                Some(Ok(())) => return Ok(0),
                None => {}
            }
            let read = self.current.read(buffer)?;
            if read > 0 || buffer.is_empty() {
                return Ok(read);
            }
            match self.next_message() {
                Ok(Message::Chunk(chunk)) => self.current = std::io::Cursor::new(chunk),
                Ok(Message::End) => self.outcome = Some(Ok(())),
                Ok(Message::Failed(error)) => {
                    self.outcome = Some(Err((error.kind(), error.to_string())))
                }
                Ok(Message::Head(_)) | Ok(Message::HeadFailed { .. }) => {}
                Err(error) => self.outcome = Some(Err((error.kind(), error.to_string()))),
            }
        }
    }
}

/// Total size of the regular files directly inside `directory`.
pub fn directory_bytes(directory: &Path) -> u64 {
    std::fs::read_dir(directory)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.metadata().ok())
        .filter(|metadata| metadata.is_file())
        .map(|metadata| metadata.len())
        .sum()
}

/// Total size of everything below `directory`.
pub fn tree_bytes(directory: &Path) -> u64 {
    std::fs::read_dir(directory)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| Some((entry.path(), entry.metadata().ok()?)))
        .map(|(path, metadata)| {
            if metadata.is_dir() {
                tree_bytes(&path)
            } else {
                metadata.len()
            }
        })
        .sum()
}

/// Whether a background manifest check is due: never checked, a full interval
/// elapsed, or the clock moved backwards past the stored time.
pub fn update_check_due(last: Option<SystemTime>, now: SystemTime) -> bool {
    match last {
        None => true,
        Some(last) => now
            .duration_since(last)
            .map_or(true, |elapsed| elapsed >= UPDATE_CHECK_INTERVAL),
    }
}

fn update_check_path(store: &InstalledDriverStore) -> PathBuf {
    store
        .base_directory()
        .join("Drivers")
        .join("v2")
        .join("last-update-check")
}

pub fn read_last_update_check(store: &InstalledDriverStore) -> Option<SystemTime> {
    let seconds: u64 = std::fs::read_to_string(update_check_path(store))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(seconds))
}

pub fn write_last_update_check(
    store: &InstalledDriverStore,
    at: SystemTime,
) -> std::io::Result<()> {
    let path = update_check_path(store);
    if let Some(directory) = path.parent() {
        std::fs::create_dir_all(directory)?;
    }
    let seconds = at
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    std::fs::write(path, format!("{seconds}\n"))
}

pub struct HttpManifestFetcher {
    max_bytes: u64,
    /// Read only the cached manifest: every fetch fails, so the registry falls
    /// back to its on-disk cache (startup, before the daily network check).
    offline: bool,
    /// The last network failure. The registry falls back to its cached
    /// manifest on a failed fetch, so this is the only place it shows.
    failure: std::sync::Mutex<Option<String>>,
}

impl HttpManifestFetcher {
    pub fn offline() -> Self {
        Self {
            max_bytes: MAX_MANIFEST_BYTES,
            offline: true,
            failure: Default::default(),
        }
    }

    /// The fetch failure seen since the last call, if any (never for the
    /// offline fetcher, which fails by design).
    pub fn take_failure(&self) -> Option<String> {
        self.failure.lock().ok()?.take()
    }
}

impl Default for HttpManifestFetcher {
    fn default() -> Self {
        Self {
            max_bytes: MAX_MANIFEST_BYTES,
            offline: false,
            failure: Default::default(),
        }
    }
}

impl engine_driver_host::ManifestFetcher for HttpManifestFetcher {
    fn fetch<'a>(
        &'a self,
        source: &'a str,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<Vec<u8>, engine_driver_host::RegistryError>>
                + Send
                + 'a,
        >,
    > {
        let source = source.to_string();
        let max_bytes = self.max_bytes;
        let offline = self.offline;
        Box::pin(async move {
            if offline {
                return Err(engine_driver_host::RegistryError::Fetch(
                    "offline: using the cached manifest".into(),
                ));
            }
            let result = smol::unblock(move || fetch_bytes(&source, max_bytes)).await;
            if let (Err(error), Ok(mut failure)) = (&result, self.failure.lock()) {
                *failure = Some(error.clone());
            }
            result.map_err(engine_driver_host::RegistryError::Fetch)
        })
    }
}

/// Fetches a manifest or signature. `file://` is readable here on purpose:
/// whether a local manifest is *acceptable* is the registry's trust check.
pub(crate) fn fetch_bytes(source: &str, max_bytes: u64) -> Result<Vec<u8>, String> {
    let url = url::Url::parse(source).map_err(|error| format!("invalid driver URL: {error}"))?;
    match url.scheme() {
        "file" => {
            let path = url
                .to_file_path()
                .map_err(|_| "invalid local driver URL".to_string())?;
            let metadata = std::fs::metadata(&path).map_err(|error| error.to_string())?;
            if metadata.len() > max_bytes {
                return Err(format!("driver response exceeds {max_bytes} bytes"));
            }
            std::fs::read(path).map_err(|error| error.to_string())
        }
        "https" | "http" => {
            if url.scheme() == "http" && !matches!(url.host_str(), Some("localhost" | "127.0.0.1"))
            {
                return Err("driver downloads require HTTPS".into());
            }
            // Never cancelled from here; the idle limit still bounds a server that goes quiet (the
            // 5 minutes are the per-wait timeout of the request, not a limit on the whole fetch).
            let (head, reader) = http_stream(
                url.as_str(),
                TrustPolicy::Strict,
                Duration::from_secs(5 * 60),
                IDLE_TIMEOUT,
                &Arc::new(AtomicBool::new(false)),
            )
            .map_err(|error| error.to_string())?;
            if !head.success {
                return Err(format!("driver download failed with HTTP {}", head.status));
            }
            if head.content_length.is_some_and(|length| length > max_bytes) {
                return Err(format!("driver response exceeds {max_bytes} bytes"));
            }
            let mut body = Vec::with_capacity(head.content_length.unwrap_or(0).min(max_bytes) as usize);
            reader
                .take(max_bytes.saturating_add(1))
                .read_to_end(&mut body)
                .map_err(|error| error.to_string())?;
            if body.len() as u64 > max_bytes {
                return Err(format!("driver response exceeds {max_bytes} bytes"));
            }
            Ok(body)
        }
        scheme => Err(format!("unsupported driver URL scheme '{scheme}'")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_driver_host::{InstallStep, ManifestTrustError, NoProgress};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);

    fn temp_store() -> (InstalledDriverStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "tablex-gpui-drivers-{}-{}",
            std::process::id(),
            TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        (InstalledDriverStore::new(&dir), dir)
    }

    #[test]
    fn temporary_download_path_preserves_zip_extension_and_lives_in_the_store() {
        let (store, dir) = temp_store();
        let zip = temporary_download_path(&store, "mongodb", "https://example.com/driver.zip?x=1");
        assert_eq!(zip.extension().and_then(|e| e.to_str()), Some("zip"));
        assert!(zip.starts_with(downloads_dir(&store)));
        let plain = temporary_download_path(&store, "mongodb", "https://example.com/driver.dylib");
        assert_eq!(plain.extension().and_then(|e| e.to_str()), Some("artifact"));
        assert_ne!(zip, plain, "every download gets its own path");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn downloads_a_local_artifact_in_developer_mode_and_reports_progress() {
        let (store, dir) = temp_store();
        let source = dir.join("source.bin");
        std::fs::write(&source, b"driver fixture").expect("write source");
        let destination = temporary_download_path(&store, "x", "file:///x.dylib");
        let seen = Mutex::new(Vec::new());
        let sink = |progress: engine_driver_host::InstallProgress| {
            seen.lock().expect("lock").push(progress)
        };
        let artifact = download_artifact(
            url::Url::from_file_path(&source).expect("url").as_str(),
            &destination,
            14,
            TrustPolicy::Developer,
            &Arc::new(AtomicBool::new(false)),
            &sink,
        )
        .expect("download fixture");
        assert_eq!(std::fs::read(&artifact.path).expect("read"), b"driver fixture");
        let seen = seen.into_inner().expect("lock");
        assert!(seen.iter().all(|p| p.step == InstallStep::Downloading));
        assert_eq!(seen.last().map(|p| p.bytes_done), Some(14));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn strict_mode_rejects_local_and_insecure_artifact_urls() {
        let (store, dir) = temp_store();
        let destination = temporary_download_path(&store, "x", "file:///x.dylib");
        for url in ["file:///tmp/x.dylib", "http://example.com/x.dylib", "ftp://h/x"] {
            let error = download_artifact(
                url,
                &destination,
                1,
                TrustPolicy::Strict,
                &Arc::new(AtomicBool::new(false)),
                &NoProgress,
            )
            .expect_err("rejected");
            assert!(
                matches!(
                    error,
                    DownloadError::LocalUrlNotAllowed(_)
                        | DownloadError::InsecureUrl(_)
                        | DownloadError::UnsupportedScheme(_)
                ),
                "{url}: {error}"
            );
        }
        assert!(!destination.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_wrong_size_or_a_cancel_leaves_no_partial_file() {
        let (store, dir) = temp_store();
        let source = dir.join("source.bin");
        std::fs::write(&source, b"driver fixture").expect("write source");
        let url = url::Url::from_file_path(&source).expect("url").to_string();
        let destination = temporary_download_path(&store, "x", &url);
        let mismatch = download_artifact(
            &url,
            &destination,
            99,
            TrustPolicy::Developer,
            &Arc::new(AtomicBool::new(false)),
            &NoProgress,
        )
        .expect_err("wrong size must fail");
        assert!(matches!(mismatch, DownloadError::SizeMismatch { .. }), "{mismatch}");
        assert!(!destination.exists());
        let cancelled = download_artifact(
            &url,
            &destination,
            14,
            TrustPolicy::Developer,
            &Arc::new(AtomicBool::new(true)),
            &NoProgress,
        )
        .expect_err("cancelled");
        assert!(matches!(cancelled, DownloadError::Cancelled));
        assert!(!destination.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn accepts_only_the_current_catalog_generation() {
        assert!(catalog_generation_is_current(4, 4));
        assert!(!catalog_generation_is_current(4, 3));
    }

    #[test]
    fn update_check_runs_at_most_once_a_day_and_survives_clock_skew() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10 * 24 * 3600);
        assert!(update_check_due(None, now));
        assert!(!update_check_due(Some(now - Duration::from_secs(3600)), now));
        assert!(!update_check_due(Some(now - UPDATE_CHECK_INTERVAL + Duration::from_secs(1)), now));
        assert!(update_check_due(Some(now - UPDATE_CHECK_INTERVAL), now));
        assert!(update_check_due(Some(now + Duration::from_secs(60)), now), "stored time in the future");
    }

    #[test]
    fn the_last_check_time_round_trips_through_the_driver_directory() {
        let (store, dir) = temp_store();
        assert_eq!(read_last_update_check(&store), None);
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        write_last_update_check(&store, at).expect("write");
        assert_eq!(read_last_update_check(&store), Some(at));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn directory_sizes_count_files_only_where_asked() {
        let (_, dir) = temp_store();
        std::fs::write(dir.join("a"), [0_u8; 10]).expect("write");
        std::fs::create_dir_all(dir.join("sub")).expect("mkdir");
        std::fs::write(dir.join("sub").join("b"), [0_u8; 5]).expect("write");
        assert_eq!(directory_bytes(&dir), 10);
        assert_eq!(tree_bytes(&dir), 15);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn local_manifest_trust_failures_name_developer_mode() {
        let error = engine_driver_host::RegistryError::Manifest(ManifestTrustError::SignatureMissing);
        let strict = explain_registry_error(&error, "file:///tmp/m.json", TrustPolicy::Strict);
        assert!(strict.contains("Developer mode"), "{strict}");
        let remote = explain_registry_error(&error, "https://h/m.json", TrustPolicy::Strict);
        assert!(!remote.contains("Developer mode"), "{remote}");
        let developer = explain_registry_error(&error, "file:///tmp/m.json", TrustPolicy::Developer);
        assert_eq!(developer, error.to_string());
    }

    #[test]
    fn a_local_manifest_failing_for_another_reason_reports_the_real_error() {
        for trust_error in [
            ManifestTrustError::SignatureInvalid,
            ManifestTrustError::SignatureMalformed("bad base64".into()),
            ManifestTrustError::MissingGeneratedAt,
            ManifestTrustError::Replayed { generated_at: "1".into() },
            ManifestTrustError::NoTrustedKeys,
        ] {
            let error = engine_driver_host::RegistryError::Manifest(trust_error);
            let said = explain_registry_error(&error, "file:///tmp/m.json", TrustPolicy::Strict);
            assert_eq!(said, error.to_string());
            assert!(!said.contains("Developer mode"), "{said}");
        }
    }

    /// A loopback HTTP server that answers its first connection with `respond` and returns the port.
    fn serve_once(respond: impl FnOnce(std::net::TcpStream) + Send + 'static) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                respond(stream);
            }
        });
        port
    }

    /// Reads the request head, so the reply is not a reset.
    fn drain_request(stream: &std::net::TcpStream) {
        use std::io::BufRead;
        let mut reader = std::io::BufReader::new(stream.try_clone().expect("clone"));
        let mut line = String::new();
        while reader.read_line(&mut line).expect("read") > 2 {
            line.clear();
        }
    }

    fn redirect_to(location: String) -> impl FnOnce(std::net::TcpStream) + Send + 'static {
        move |mut stream| {
            use std::io::Write;
            drain_request(&stream);
            let _ = write!(stream, "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        }
    }

    fn never() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

    fn url_of(text: &str) -> url::Url {
        url::Url::parse(text).expect("url")
    }

    #[test]
    fn a_redirect_never_leaves_https_and_every_hop_is_classified() {
        for policy in [TrustPolicy::Strict, TrustPolicy::Developer] {
            let https = url_of("https://downloads.example.com/a");
            // https -> plain http: refused, loopback or not, under both policies.
            for next in ["http://127.0.0.1:9/a", "http://localhost/a", "http://example.com/a", "ftp://example.com/a"] {
                let refused = redirect_allowed(Some(&https), &url_of(next), policy).expect_err(next);
                assert!(refused.contains("refusing a redirect"), "{next}: {refused}");
            }
            assert!(redirect_allowed(Some(&https), &url_of("https://cdn.example.com/a"), policy).is_ok());
            // A first URL of plain-http loopback may redirect to loopback http (and to https), but
            // never to a remote plain-http host.
            let local = url_of("http://127.0.0.1:9/a");
            assert!(redirect_allowed(Some(&local), &url_of("http://127.0.0.1:10/b"), policy).is_ok());
            assert!(redirect_allowed(Some(&local), &url_of("https://cdn.example.com/b"), policy).is_ok());
            assert!(redirect_allowed(Some(&local), &url_of("http://example.com/b"), policy).is_err());
            // No previous hop (not expected): the target is classified all the same.
            assert!(redirect_allowed(None, &url_of("http://example.com/b"), policy).is_err());
            assert!(redirect_allowed(None, &url_of("https://cdn.example.com/b"), policy).is_ok());
        }
    }

    #[test]
    fn a_consumer_that_was_stopped_longer_than_the_idle_time_still_gets_what_is_waiting() {
        let script = Script([Ok(b"ab".to_vec()), Ok(b"cd".to_vec())].into());
        let mut reader = IdleReader::of(script, Duration::from_millis(100), never());
        // The consumer is busy for four times the idle time; the helper has queued the chunks.
        std::thread::sleep(Duration::from_millis(400));
        let mut body = Vec::new();
        reader.read_to_end(&mut body).expect("a slow consumer is not a stalled server");
        assert_eq!(body, b"abcd");
    }

    #[test]
    fn a_refused_redirect_is_a_typed_error_and_loopback_chains_still_work() {
        // A loopback http server redirecting to a remote plain-http host: refused by the hop
        // classification, as the typed `InsecureUrl`.
        let port = serve_once(redirect_to("http://example.com/artifact".into()));
        let error = open_artifact(
            &format!("http://127.0.0.1:{port}/artifact"),
            100,
            DownloadTransport::Http,
            TrustPolicy::Strict,
            &never(),
        )
        .err()
        .expect("a redirect off the policy must fail");
        assert!(matches!(error, DownloadError::InsecureUrl(_)), "{error:?}");
        assert!(error.to_string().contains("refusing a redirect"), "{error}");

        // http loopback -> http loopback is followed. (The https -> http-loopback case is covered
        // on the policy function above: a local https server needs a certificate the client
        // would have to trust, which is not practical in a test.)
        let body = b"the artifact".to_vec();
        let served = body.clone();
        let target = serve_once(move |mut stream| {
            use std::io::Write;
            drain_request(&stream);
            let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", served.len());
            let _ = stream.write_all(&served);
        });
        let first = serve_once(redirect_to(format!("http://127.0.0.1:{target}/final")));
        let mut reader = open_artifact(
            &format!("http://127.0.0.1:{first}/artifact"),
            100,
            DownloadTransport::Http,
            TrustPolicy::Strict,
            &never(),
        )
        .expect("a loopback redirect is followed");
        let mut received = Vec::new();
        reader.read_to_end(&mut received).expect("read");
        assert_eq!(received, body);
    }

    #[test]
    fn a_stalled_body_fails_within_the_idle_time_and_what_arrived_is_delivered_first() {
        let port = serve_once(|mut stream| {
            use std::io::Write;
            drain_request(&stream);
            let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n");
            let _ = stream.write_all(&[7_u8; 100]);
            let _ = stream.flush();
            std::thread::sleep(Duration::from_secs(20));
        });
        let started = std::time::Instant::now();
        let mut reader = open_artifact_idle(
            &format!("http://127.0.0.1:{port}/artifact"),
            1000,
            DownloadTransport::Http,
            TrustPolicy::Strict,
            Duration::from_millis(400),
            &never(),
        )
        .expect("headers arrive");
        let mut received = Vec::new();
        let error = reader.read_to_end(&mut received).expect_err("the body never completes");
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut, "{error}");
        assert_eq!(received.len(), 100);
        assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
        // Sticky: a later read is an error again, never end of file.
        assert_eq!(reader.read(&mut [0_u8; 8]).expect_err("still failed").kind(), std::io::ErrorKind::TimedOut);

        // Through the real download path the stall is an I/O failure and the file is gone.
        let (store, dir) = temp_store();
        let port = serve_once(|mut stream| {
            use std::io::Write;
            drain_request(&stream);
            let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n");
            let _ = stream.write_all(&[7_u8; 100]);
            let _ = stream.flush();
            std::thread::sleep(Duration::from_secs(5));
        });
        let destination = temporary_download_path(&store, "x", "http://h/x");
        let mut input = open_artifact_idle(
            &format!("http://127.0.0.1:{port}/artifact"),
            1000,
            DownloadTransport::Http,
            TrustPolicy::Strict,
            Duration::from_millis(400),
            &never(),
        )
        .expect("headers arrive");
        std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
        let result = stream_to_file(&mut *input, &destination, 1000, &never(), &NoProgress);
        assert!(matches!(result, Err(DownloadError::Io(_))), "{result:?}");
        assert!(!destination.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_server_that_accepts_and_never_answers_fails_within_the_idle_time() {
        // Accepts, reads the request, then says nothing at all (no head).
        let port = serve_once(|stream| {
            drain_request(&stream);
            std::thread::sleep(Duration::from_secs(20));
        });
        let started = std::time::Instant::now();
        let error = open_artifact_idle(
            &format!("http://127.0.0.1:{port}/artifact"),
            1000,
            DownloadTransport::Http,
            TrustPolicy::Strict,
            Duration::from_millis(500),
            &never(),
        )
        .err()
        .expect("no head, no download");
        assert!(matches!(&error, DownloadError::Io(e) if e.kind() == std::io::ErrorKind::TimedOut), "{error:?}");
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    }

    #[test]
    fn cancel_returns_promptly_during_a_header_stall_and_during_a_body_stall() {
        // Header stall: the request is accepted and never answered; the idle limit is far away.
        let port = serve_once(|stream| {
            drain_request(&stream);
            std::thread::sleep(Duration::from_secs(30));
        });
        let cancel = never();
        let raise = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            raise.store(true, Ordering::Release);
        });
        let started = std::time::Instant::now();
        let error = open_artifact_idle(
            &format!("http://127.0.0.1:{port}/artifact"),
            1000,
            DownloadTransport::Http,
            TrustPolicy::Strict,
            Duration::from_secs(60),
            &cancel,
        )
        .err()
        .expect("cancelled");
        assert!(matches!(error, DownloadError::Cancelled), "{error:?}");
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());

        // Body stall: the head and some bytes arrive, then nothing.
        let port = serve_once(|mut stream| {
            use std::io::Write;
            drain_request(&stream);
            let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n");
            let _ = stream.write_all(&[7_u8; 100]);
            let _ = stream.flush();
            std::thread::sleep(Duration::from_secs(30));
        });
        let cancel = never();
        let mut reader = open_artifact_idle(
            &format!("http://127.0.0.1:{port}/artifact"),
            1000,
            DownloadTransport::Http,
            TrustPolicy::Strict,
            Duration::from_secs(60),
            &cancel,
        )
        .expect("headers arrive");
        let raise = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            raise.store(true, Ordering::Release);
        });
        let started = std::time::Instant::now();
        let mut received = Vec::new();
        let error = reader.read_to_end(&mut received).expect_err("cancelled");
        assert!(error.to_string().contains("cancelled"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
    }

    /// Yields the scripted results one `read` at a time.
    struct Script(std::collections::VecDeque<std::io::Result<Vec<u8>>>);

    impl Read for Script {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            match self.0.pop_front() {
                None => Ok(0),
                Some(Ok(bytes)) => {
                    buffer[..bytes.len()].copy_from_slice(&bytes);
                    Ok(bytes.len())
                }
                Some(Err(error)) => Err(error),
            }
        }
    }

    #[test]
    fn interrupted_reads_are_retried_and_a_failure_is_never_a_short_body() {
        use std::io::{Error, ErrorKind};
        let interrupted = || Err(Error::from(ErrorKind::Interrupted));
        // Interrupted, data, interrupted, data: the whole body arrives.
        let script = Script([interrupted(), Ok(b"ab".to_vec()), interrupted(), Ok(b"cd".to_vec())].into());
        let mut body = Vec::new();
        IdleReader::of(script, Duration::from_secs(5), never()).read_to_end(&mut body).expect("complete");
        assert_eq!(body, b"abcd");

        // A failure in the middle: what came before is delivered, then the error, and every later
        // read is the error again (never end of file).
        let script = Script([Ok(b"ab".to_vec()), Err(Error::new(ErrorKind::ConnectionReset, "reset"))].into());
        let mut reader = IdleReader::of(script, Duration::from_secs(5), never());
        let mut body = Vec::new();
        let error = reader.read_to_end(&mut body).expect_err("a truncated body is an error");
        assert_eq!((body.as_slice(), error.kind()), (&b"ab"[..], ErrorKind::ConnectionReset));
        for _ in 0..3 {
            assert_eq!(reader.read(&mut [0_u8; 4]).expect_err("sticky").kind(), ErrorKind::ConnectionReset);
        }

        // A helper that vanishes without `End` is an error too, not an empty rest of the body.
        let (sender, messages) = std::sync::mpsc::sync_channel(1);
        sender.send(Message::Chunk(b"xy".to_vec())).unwrap();
        drop(sender);
        let mut reader = IdleReader::new(messages, Duration::from_secs(5), never());
        let mut body = Vec::new();
        let error = reader.read_to_end(&mut body).expect_err("no End, no EOF");
        assert_eq!((body.as_slice(), error.kind()), (&b"xy"[..], ErrorKind::BrokenPipe));
    }
}
