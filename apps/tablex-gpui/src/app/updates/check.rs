//! The update check without any UI: release parsing, version order, the decision and the
//! persisted check state. The network sits behind [`ReleaseFetcher`], so tests feed canned answers.

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::fmt;
use std::io::Read as _;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

/// The release marked Latest on GitHub: always the app's own (driver releases are `drivers-v*`
/// tags and never carry the Latest label).
pub(in crate::app) const LATEST_RELEASE_URL: &str =
    "https://api.github.com/repos/tablex-org/tablex/releases/latest";

/// Where Download goes when the answer carries no usable page address.
const RELEASES_PAGE: &str = "https://github.com/tablex-org/tablex/releases/latest";

/// Held by every load-modify-save of the state file, so a check and a Skip This Version cannot overwrite each other.
static STATE_LOCK: Mutex<()> = Mutex::new(());

/// The answer is a few KiB of JSON; anything past this is not one.
const MAX_ANSWER_BYTES: u64 = 256 * 1024;

/// A semantic version (`major.minor.patch[-pre]`); build metadata is dropped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::app) struct Version {
    core: [u64; 3],
    pre: Option<String>,
}

impl Version {
    pub(in crate::app) fn parse(text: &str) -> Option<Self> {
        let text = text.split_once('+').map_or(text, |(version, _build)| version);
        let (core, pre) = match text.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (text, None),
        };
        let mut parts = core.split('.');
        let mut number = || {
            let part = parts.next()?;
            let plain = !part.is_empty()
                && part.bytes().all(|b| b.is_ascii_digit())
                && (part == "0" || !part.starts_with('0'));
            plain.then(|| part.parse().ok()).flatten()
        };
        let core = [number()?, number()?, number()?];
        if parts.next().is_some() {
            return None;
        }
        let pre = match pre {
            None => None,
            Some(pre) => {
                let valid = pre.split('.').all(|id| {
                    !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                });
                if !valid {
                    return None;
                }
                Some(pre.to_owned())
            }
        };
        Some(Self { core, pre })
    }

    pub(in crate::app) fn is_prerelease(&self) -> bool {
        self.pre.is_some()
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [major, minor, patch] = self.core;
        write!(f, "{major}.{minor}.{patch}")?;
        match &self.pre {
            Some(pre) => write!(f, "-{pre}"),
            None => Ok(()),
        }
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.core.cmp(&other.core).then_with(|| match (&self.pre, &other.pre) {
            (None, None) => Ordering::Equal,
            // A prerelease comes before its release.
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some(a), Some(b)) => cmp_prerelease(a, b),
        })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Dot-separated identifiers: numbers by value and before words, words by text, a prefix first.
fn cmp_prerelease(a: &str, b: &str) -> Ordering {
    let numeric = |id: &str| id.bytes().all(|b| b.is_ascii_digit());
    let (mut a, mut b) = (a.split('.'), b.split('.'));
    loop {
        let order = match (a.next(), b.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => match (numeric(x), numeric(y)) {
                // No leading zeros: a longer number is a bigger one.
                (true, true) => x.len().cmp(&y.len()).then_with(|| x.cmp(y)),
                (true, false) => Ordering::Less,
                (false, true) => Ordering::Greater,
                (false, false) => x.cmp(y),
            },
        };
        if order != Ordering::Equal {
            return order;
        }
    }
}

/// A published app release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::app) struct Release {
    pub(in crate::app) version: Version,
    /// The release page (`html_url`), opened by Download.
    pub(in crate::app) url: String,
}

/// Reads GitHub's "latest release" answer. `Ok(None)`: the release is not an app release
/// (a tag that is not `v<semver>`), which is "no update". `Err`: not an answer to this question.
pub(in crate::app) fn parse_latest(body: &str) -> Result<Option<Release>, String> {
    #[derive(Deserialize)]
    struct Wire {
        tag_name: String,
        #[serde(default)]
        html_url: String,
    }
    let wire: Wire =
        serde_json::from_str(body).map_err(|error| format!("unexpected answer ({error})"))?;
    let Some(version) = wire.tag_name.strip_prefix('v').and_then(Version::parse) else {
        return Ok(None);
    };
    // The address is opened in the browser: only an https page on github.com is taken from the answer.
    let url = match url::Url::parse(&wire.html_url) {
        Ok(url) if url.scheme() == "https" && url.host_str() == Some("github.com") => wire.html_url,
        _ => RELEASES_PAGE.to_owned(),
    };
    Ok(Some(Release { version, url }))
}

/// Gets the body of the latest-release answer.
pub(in crate::app) trait ReleaseFetcher: Send + Sync {
    fn fetch(&self) -> Result<String, String>;
}

/// The production fetcher: one HTTPS GET to GitHub, no identifier of any kind in it.
pub(in crate::app) struct HttpReleaseFetcher;

impl ReleaseFetcher for HttpReleaseFetcher {
    fn fetch(&self) -> Result<String, String> {
        let describe = |error: zed_reqwest::Error| {
            let mut text = error.to_string();
            let mut source = std::error::Error::source(&error);
            while let Some(cause) = source {
                text.push_str(": ");
                text.push_str(&cause.to_string());
                source = cause.source();
            }
            text
        };
        let client = zed_reqwest::blocking::Client::builder()
            .user_agent(format!("TableX/{}", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Some(Duration::from_secs(20)))
            .https_only(true)
            .redirect(zed_reqwest::redirect::Policy::limited(3))
            .build()
            .map_err(describe)?;
        let response = client
            .get(LATEST_RELEASE_URL)
            .header("Accept", "application/vnd.github+json")
            .send()
            .map_err(describe)?;
        if !response.status().is_success() {
            return Err(format!("GitHub answered {}", response.status()));
        }
        let mut body = String::new();
        response
            .take(MAX_ANSWER_BYTES)
            .read_to_string(&mut body)
            .map_err(|error| error.to_string())?;
        Ok(body)
    }
}

/// Scenarios and tests never reach the network.
pub(in crate::app) struct OfflineFetcher;

impl ReleaseFetcher for OfflineFetcher {
    fn fetch(&self) -> Result<String, String> {
        Err("offline".into())
    }
}

/// What survives a restart: when the last check ran and the version the user chose to skip.
/// Kept in its own small file, not in `settings.json` (a clock and a click are not settings).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(in crate::app) struct State {
    /// Unix seconds.
    pub(in crate::app) last_check: Option<u64>,
    pub(in crate::app) skipped: Option<String>,
}

impl State {
    /// A missing or unreadable file is a fresh state: the worst case is one more check.
    pub(in crate::app) fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub(in crate::app) fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(directory) = path.parent() {
            std::fs::create_dir_all(directory)?;
        }
        // A name of its own: a writer in another process never renames the file this one is writing.
        let staged = path.with_extension(format!("json.{}.tmp", uuid::Uuid::new_v4()));
        std::fs::write(&staged, serde_json::to_vec(self).map_err(std::io::Error::other)?)?;
        std::fs::rename(&staged, path).inspect_err(|_| {
            let _ = std::fs::remove_file(&staged);
        })
    }

    /// `None`: never checked, or a time that cannot be (a damaged file): a check is due.
    fn last_check_time(&self) -> Option<SystemTime> {
        SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(self.last_check?))
    }
}

/// Remembers `version` as the one not to announce again.
pub(in crate::app) fn skip_version(path: &Path, version: &Version) -> std::io::Result<()> {
    let _guard = STATE_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut state = State::load(path);
    state.skipped = Some(version.to_string());
    state.save(path)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::app) enum Mode {
    /// On launch: throttled, honours the skipped version, silent about everything but news.
    Auto,
    /// On demand: always checks and always reports.
    Manual,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::app) enum Outcome {
    /// Auto only: the last check was less than a day ago, nothing was fetched.
    NotDue,
    UpToDate,
    Available(Release),
    Failed(String),
}

/// Whether the launch check may run at all: the preference is on, this is not a debug build and
/// this build carries a release version (not `dev`, not a prerelease).
pub(in crate::app) fn auto_check_wanted(enabled: bool, debug_build: bool, current: &str) -> bool {
    enabled
        && !debug_build
        && Version::parse(current).is_some_and(|version| !version.is_prerelease())
}

/// Runs one check (blocking; call it off the UI thread).
pub(in crate::app) fn run(
    fetcher: &dyn ReleaseFetcher,
    state_path: Option<&Path>,
    current: &str,
    mode: Mode,
    now: SystemTime,
) -> Outcome {
    let Some(current) = Version::parse(current) else {
        return Outcome::Failed("this build has no release version".into());
    };
    let state = {
        let _guard = STATE_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut state = state_path.map(State::load).unwrap_or_default();
        if mode == Mode::Auto && !crate::drivers::update_check_due(state.last_check_time(), now) {
            return Outcome::NotDue;
        }
        // The attempt counts, whether or not it succeeds: at most one automatic fetch a day.
        state.last_check = Some(now.duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs()));
        if let Some(path) = state_path {
            let _ = state.save(path);
        }
        state
    };
    match fetcher.fetch().and_then(|body| parse_latest(&body)) {
        Err(error) => Outcome::Failed(error),
        Ok(Some(release)) if release.version > current => {
            let skipped = state.skipped.as_deref() == Some(release.version.to_string().as_str());
            if skipped && mode == Mode::Auto {
                Outcome::UpToDate
            } else {
                Outcome::Available(release)
            }
        }
        Ok(_) => Outcome::UpToDate,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

    struct Canned {
        answer: Result<String, String>,
        calls: AtomicUsize,
    }

    impl Canned {
        fn new(answer: Result<String, String>) -> Self {
            Self { answer, calls: AtomicUsize::new(0) }
        }

        fn tag(tag: &str) -> Self {
            Self::new(Ok(body(tag)))
        }
    }

    impl ReleaseFetcher for Canned {
        fn fetch(&self) -> Result<String, String> {
            self.calls.fetch_add(1, SeqCst);
            self.answer.clone()
        }
    }

    fn body(tag: &str) -> String {
        serde_json::json!({
            "tag_name": tag,
            "html_url": format!("https://github.com/tablex-org/tablex/releases/tag/{tag}"),
            "name": "ignored",
        })
        .to_string()
    }

    fn at(seconds: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
    }

    fn version(text: &str) -> Version {
        Version::parse(text).unwrap_or_else(|| panic!("{text} is a version"))
    }

    const DAY: u64 = 24 * 60 * 60;

    fn temp_state() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("tablex-update-{}", uuid::Uuid::new_v4())).join("update-check.json")
    }

    #[test]
    fn versions_are_compared_by_number_not_by_text() {
        assert!(version("0.10.0") > version("0.9.0"));
        assert!(version("1.0.0") > version("0.99.99"));
        assert!(version("0.1.1") > version("0.1.0"));
        assert_eq!(version("0.1.0"), version("0.1.0+build.5"));
        assert!(version("1.0.0-rc.1") < version("1.0.0"));
        assert!(version("1.0.0-rc.2") < version("1.0.0-rc.10"));
        assert!(version("1.0.0-rc") < version("1.0.0-rc.1"));
        assert!(version("1.0.0-1") < version("1.0.0-alpha"));
    }

    #[test]
    fn only_complete_semver_is_a_version() {
        for bad in ["", "1", "1.2", "1.2.3.4", "01.2.3", "1.2.x", "v1.2.3", "dev", "1.2.3-", "1.2.3-a..b", "-1.2.3"] {
            assert!(Version::parse(bad).is_none(), "{bad:?}");
        }
        assert_eq!(version("1.2.3-rc.1").to_string(), "1.2.3-rc.1");
        assert!(version("1.2.3-rc.1").is_prerelease() && !version("1.2.3").is_prerelease());
    }

    #[test]
    fn a_newer_release_is_available_an_equal_or_older_one_is_not() {
        let fetcher = Canned::tag("v0.2.0");
        match run(&fetcher, None, "0.1.0", Mode::Manual, at(DAY)) {
            Outcome::Available(release) => {
                assert_eq!(release.version, version("0.2.0"));
                assert_eq!(release.url, "https://github.com/tablex-org/tablex/releases/tag/v0.2.0");
            }
            other => panic!("{other:?}"),
        }
        for tag in ["v0.1.0", "v0.0.9"] {
            let outcome = run(&Canned::tag(tag), None, "0.1.0", Mode::Manual, at(DAY));
            assert_eq!(outcome, Outcome::UpToDate, "{tag}");
        }
        // Numbers, not text: 0.10.0 is newer than 0.9.0.
        assert!(matches!(run(&Canned::tag("v0.10.0"), None, "0.9.0", Mode::Manual, at(DAY)), Outcome::Available(_)));
    }

    #[test]
    fn a_driver_tag_or_a_malformed_tag_means_no_update() {
        for tag in ["drivers-v2026.11.05", "drivers-v9.9.9", "v1.2", "1.2.3", "release-1", "vx.y.z", ""] {
            let outcome = run(&Canned::tag(tag), None, "0.1.0", Mode::Manual, at(DAY));
            assert_eq!(outcome, Outcome::UpToDate, "{tag:?}");
        }
        assert_eq!(parse_latest(&body("drivers-v1.0.0")), Ok(None));
    }

    #[test]
    fn an_answer_that_is_not_a_release_is_a_failure() {
        assert!(parse_latest("not json").is_err());
        assert!(parse_latest(r#"{"message":"Not Found"}"#).is_err());
        let outcome = run(&Canned::new(Err("offline".into())), None, "0.1.0", Mode::Manual, at(DAY));
        assert_eq!(outcome, Outcome::Failed("offline".into()));
    }

    #[test]
    fn download_opens_https_pages_only() {
        let fallback = |url: &str| {
            let body = serde_json::json!({"tag_name": "v1.0.0", "html_url": url}).to_string();
            parse_latest(&body).unwrap().unwrap().url
        };
        assert_eq!(fallback("https://github.com/x/y"), "https://github.com/x/y");
        for unsafe_url in ["http://github.com/x/y", "file:///etc/passwd", "javascript:alert(1)", "", "github.com"] {
            assert_eq!(fallback(unsafe_url), RELEASES_PAGE, "{unsafe_url:?}");
        }
    }

    #[test]
    fn only_a_github_page_is_opened_whatever_else_https_says() {
        let url = |html_url: &str| {
            let body = serde_json::json!({"tag_name": "v1.0.0", "html_url": html_url}).to_string();
            parse_latest(&body).unwrap().unwrap().url
        };
        assert_eq!(url("https://github.com/tablex-org/tablex/releases/tag/v1.0.0"), "https://github.com/tablex-org/tablex/releases/tag/v1.0.0");
        for other in [
            "https://evil.example/releases",
            "https://github.com.evil.example/x",
            "https://api.github.com/x",
            "https://user@evil.example/x",
            "https://evil.example/github.com",
        ] {
            assert_eq!(url(other), RELEASES_PAGE, "{other:?}");
        }
    }

    #[test]
    fn a_huge_last_check_in_a_damaged_state_file_means_a_check_is_due_and_does_not_panic() {
        let path = temp_state();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"last_check":18446744073709551615}"#).unwrap();
        let state = State::load(&path);
        assert_eq!(state.last_check, Some(u64::MAX));
        assert_eq!(state.last_check_time(), None);
        let fetcher = Canned::tag("v0.2.0");
        let outcome = run(&fetcher, Some(&path), "0.1.0", Mode::Auto, at(DAY));
        assert!(matches!(outcome, Outcome::Available(_)), "{outcome:?}");
        assert_eq!(fetcher.calls.load(SeqCst), 1);
        assert_eq!(State::load(&path).last_check, Some(DAY), "and the state is whole again");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn checks_and_skips_at_the_same_time_lose_neither_write() {
        let path = temp_state();
        let threads: Vec<_> = (0..8)
            .map(|n| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for round in 0..10 {
                        if n % 2 == 0 {
                            run(&Canned::tag("v0.1.0"), Some(&path), "0.1.0", Mode::Manual, at((n * 100 + round + 1) * DAY));
                        } else {
                            skip_version(&path, &version("0.2.0")).unwrap();
                        }
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let state = State::load(&path);
        assert_eq!(state.skipped.as_deref(), Some("0.2.0"), "a check never wrote the skip away");
        assert!(state.last_check.is_some());
        let left: Vec<_> = std::fs::read_dir(path.parent().unwrap()).unwrap().map(|entry| entry.unwrap().file_name()).collect();
        assert_eq!(left.len(), 1, "no staging file is left behind: {left:?}");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_skipped_version_is_not_announced_again_but_a_manual_check_still_shows_it() {
        let path = temp_state();
        skip_version(&path, &version("0.2.0")).unwrap();
        let fetcher = Canned::tag("v0.2.0");
        let outcome = run(&fetcher, Some(&path), "0.1.0", Mode::Auto, at(10 * DAY));
        assert_eq!(outcome, Outcome::UpToDate, "skipped");
        assert_eq!(fetcher.calls.load(SeqCst), 1);
        let outcome = run(&fetcher, Some(&path), "0.1.0", Mode::Manual, at(10 * DAY));
        assert!(matches!(outcome, Outcome::Available(_)), "{outcome:?}");
        // Only that version: the next one is announced.
        let outcome = run(&Canned::tag("v0.2.1"), Some(&path), "0.1.0", Mode::Auto, at(20 * DAY));
        assert!(matches!(outcome, Outcome::Available(_)), "{outcome:?}");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn an_automatic_check_runs_once_in_24_hours_a_manual_one_always() {
        let path = temp_state();
        let fetcher = Canned::tag("v0.2.0");
        let check = |mode, now| run(&fetcher, Some(&path), "0.1.0", mode, at(now));
        assert!(matches!(check(Mode::Auto, 5 * DAY), Outcome::Available(_)), "never checked: due");
        assert_eq!(State::load(&path).last_check, Some(5 * DAY), "the time is persisted");
        assert_eq!(check(Mode::Auto, 5 * DAY + DAY - 1), Outcome::NotDue);
        assert_eq!(fetcher.calls.load(SeqCst), 1, "not due: nothing fetched");
        assert!(matches!(check(Mode::Manual, 5 * DAY + 60), Outcome::Available(_)), "manual ignores the throttle");
        assert_eq!(fetcher.calls.load(SeqCst), 2);
        assert!(matches!(check(Mode::Auto, 6 * DAY + 60), Outcome::Available(_)), "a day after the last attempt");
        assert_eq!(fetcher.calls.load(SeqCst), 3);
        // A failed attempt counts too.
        let failing = Canned::new(Err("offline".into()));
        assert!(matches!(run(&failing, Some(&path), "0.1.0", Mode::Auto, at(8 * DAY)), Outcome::Failed(_)));
        assert_eq!(run(&failing, Some(&path), "0.1.0", Mode::Auto, at(8 * DAY + 1)), Outcome::NotDue);
        assert_eq!(failing.calls.load(SeqCst), 1);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_damaged_state_file_is_a_fresh_state() {
        let path = temp_state();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(State::load(&path), State::default());
        std::fs::write(&path, r#"{"skipped":"0.3.0","future_field":1}"#).unwrap();
        assert_eq!(State::load(&path).skipped.as_deref(), Some("0.3.0"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn the_automatic_check_needs_the_preference_a_release_build_and_a_release_version() {
        assert!(auto_check_wanted(true, false, "0.1.0"));
        assert!(!auto_check_wanted(false, false, "0.1.0"), "preference off");
        assert!(!auto_check_wanted(true, true, "0.1.0"), "debug build");
        assert!(!auto_check_wanted(true, false, "dev"), "dev version");
        assert!(!auto_check_wanted(true, false, "0.2.0-rc.1"), "prerelease");
        assert!(!auto_check_wanted(true, false, ""));
    }

    #[test]
    fn a_build_without_a_release_version_cannot_compare() {
        let fetcher = Canned::tag("v9.9.9");
        let outcome = run(&fetcher, None, "dev", Mode::Manual, at(DAY));
        assert!(matches!(outcome, Outcome::Failed(_)));
        assert_eq!(fetcher.calls.load(SeqCst), 0);
    }
}
