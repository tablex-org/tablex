use super::*;
use base64::Engine as _;
use sha2::Digest as _;
use ed25519_dalek::{Signer as _, SigningKey};
use engine_driver_host::TrustPolicy;
use std::collections::HashMap;
use std::sync::Mutex;

const AARCH64: &str = "aarch64-apple-darwin";
const X86_64: &str = "x86_64-apple-darwin";

pub(in crate::app) fn v(text: &str) -> Version {
    Version::parse(text).unwrap()
}

pub(in crate::app) fn key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

pub(in crate::app) fn trust(key: &SigningKey) -> ManifestTrust {
    ManifestTrust::new(&[key.verifying_key().to_bytes()], TrustPolicy::Strict).unwrap()
}

pub(in crate::app) fn sign(key: &SigningKey, body: &[u8]) -> Vec<u8> {
    let signature = base64::engine::general_purpose::STANDARD.encode(key.sign(body).to_bytes());
    format!("{signature}\n").into_bytes()
}

const URL: &str = "https://github.com/tablex-org/tablex/releases/download/v0.2.0/TableX-0.2.0-macos-apple-silicon.app.zip";
const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn manifest_json(version: &str) -> serde_json::Value {
    serde_json::json!({
        "kind": KIND,
        "schema": 1,
        "version": version,
        "generatedAt": "2026-11-11T00:00:00Z",
        "artifacts": [
            {"target": AARCH64, "url": URL, "size": 1234, "sha256": SHA.to_uppercase()},
            {"target": X86_64, "url": "https://example.com/intel.zip", "size": 99, "sha256": SHA},
        ],
    })
}

fn verify(key_used: &SigningKey, json: &serde_json::Value, release: &str, current: &str, target: &str) -> Result<Artifact, ManifestError> {
    let body = json.to_string().into_bytes();
    let signature = sign(key_used, &body);
    verify_manifest(&trust(&key(1)), &body, &signature, &v(release), &v(current), target)
}

// --- the manifest -----------------------------------------------------------------------------

#[test]
fn a_manifest_signed_by_the_pinned_key_gives_the_artifact_of_this_target() {
    let artifact = verify(&key(1), &manifest_json("0.2.0"), "0.2.0", "0.1.4", AARCH64).unwrap();
    assert_eq!(artifact, Artifact { url: URL.into(), size: 1234, sha256: SHA.into() }, "the hash is normalized to lower case");
    let intel = verify(&key(1), &manifest_json("0.2.0"), "0.2.0", "0.1.4", X86_64).unwrap();
    assert_eq!(intel.url, "https://example.com/intel.zip");
}

#[test]
fn a_signature_by_another_key_or_over_other_bytes_is_rejected() {
    let error = verify(&key(2), &manifest_json("0.2.0"), "0.2.0", "0.1.4", AARCH64).unwrap_err();
    assert!(matches!(error, ManifestError::Signature(_)), "{error}");

    let body = manifest_json("0.2.0").to_string().into_bytes();
    let signature = sign(&key(1), &body);
    let mut tampered = body.clone();
    tampered.extend_from_slice(b" ");
    let error = verify_manifest(&trust(&key(1)), &tampered, &signature, &v("0.2.0"), &v("0.1.4"), AARCH64).unwrap_err();
    assert!(matches!(error, ManifestError::Signature(_)), "{error}");

    for missing in [&b""[..], b"not base64 !!"] {
        let error = verify_manifest(&trust(&key(1)), &body, missing, &v("0.2.0"), &v("0.1.4"), AARCH64).unwrap_err();
        assert!(matches!(error, ManifestError::Signature(_)), "{error}");
    }
}

#[test]
fn nothing_of_an_unsigned_manifest_is_read() {
    // Garbage with a bad signature is a signature failure, not a parse failure: the fields come second.
    let signature = sign(&key(2), b"{}");
    let error = verify_manifest(&trust(&key(1)), b"not json", &signature, &v("0.2.0"), &v("0.1.4"), AARCH64).unwrap_err();
    assert!(matches!(error, ManifestError::Signature(_)), "{error}");
    // A build that pins no key trusts nothing.
    let none = ManifestTrust::new(&[], TrustPolicy::Strict).unwrap();
    let body = manifest_json("0.2.0").to_string().into_bytes();
    let signature = sign(&key(1), &body);
    let error = verify_manifest(&none, &body, &signature, &v("0.2.0"), &v("0.1.4"), AARCH64).unwrap_err();
    assert!(matches!(error, ManifestError::Signature(_)), "{error}");
}

#[test]
fn a_signed_manifest_for_another_version_is_refused() {
    let error = verify(&key(1), &manifest_json("0.3.0"), "0.2.0", "0.1.4", AARCH64).unwrap_err();
    assert_eq!(error, ManifestError::VersionMismatch { manifest: "0.3.0".into(), release: "0.2.0".into() });
}

#[test]
fn an_equal_or_older_version_is_never_installed() {
    let same = verify(&key(1), &manifest_json("0.2.0"), "0.2.0", "0.2.0", AARCH64).unwrap_err();
    assert!(matches!(same, ManifestError::NotNewer { .. }), "{same}");
    let older = verify(&key(1), &manifest_json("0.1.0"), "0.1.0", "0.2.0", AARCH64).unwrap_err();
    assert!(matches!(older, ManifestError::NotNewer { .. }), "{older}");
}

#[test]
fn a_manifest_without_a_build_for_this_target_is_refused() {
    let error = verify(&key(1), &manifest_json("0.2.0"), "0.2.0", "0.1.4", "aarch64-unknown-linux-gnu").unwrap_err();
    assert_eq!(error, ManifestError::NoArtifact("aarch64-unknown-linux-gnu".into()));
}

#[test]
fn a_signed_manifest_of_another_kind_or_with_a_bad_artifact_is_refused() {
    let mut driver_catalog = manifest_json("0.2.0");
    driver_catalog["schema"] = 2.into();
    assert_eq!(verify(&key(1), &driver_catalog, "0.2.0", "0.1.4", AARCH64).unwrap_err(), ManifestError::Schema(2));
    // The driver catalog (signed by the same key) has no `kind` and no `schema` at all.
    let catalog = serde_json::json!({"schemaVersion": 2, "drivers": [], "tools": []});
    let error = verify(&key(1), &catalog, "0.2.0", "0.1.4", AARCH64).unwrap_err();
    assert!(matches!(error, ManifestError::Malformed(_)), "{error}");

    for kind in [serde_json::json!("tablex-driver-catalog"), serde_json::json!(7), serde_json::Value::Null] {
        let mut other = manifest_json("0.2.0");
        other["kind"] = kind;
        let error = verify(&key(1), &other, "0.2.0", "0.1.4", AARCH64).unwrap_err();
        assert!(matches!(error, ManifestError::Malformed(_)), "{error}");
    }
    let mut without_kind = manifest_json("0.2.0");
    without_kind.as_object_mut().unwrap().remove("kind");
    assert!(matches!(verify(&key(1), &without_kind, "0.2.0", "0.1.4", AARCH64).unwrap_err(), ManifestError::Malformed(_)));

    for (field, value) in [
        ("url", serde_json::json!("http://example.com/a.zip")),
        ("url", serde_json::json!("file:///tmp/a.zip")),
        ("size", serde_json::json!(0)),
        ("size", serde_json::json!(MAX_ARTIFACT_BYTES + 1)),
        ("sha256", serde_json::json!("abc")),
        ("sha256", serde_json::json!("zz".repeat(32))),
    ] {
        let mut json = manifest_json("0.2.0");
        json["artifacts"][0][field] = value;
        let error = verify(&key(1), &json, "0.2.0", "0.1.4", AARCH64).unwrap_err();
        assert!(matches!(error, ManifestError::BadArtifact(_)), "{field}: {error}");
    }
}

/// Serves canned bodies by address and records what was asked.
#[derive(Default)]
pub(in crate::app) struct Fake {
    files: Mutex<HashMap<String, Vec<u8>>>,
    asked: Mutex<Vec<String>>,
    /// While set, a download waits (until it is cancelled).
    pub(in crate::app) hold: AtomicBool,
}

impl Fake {
    pub(in crate::app) fn serve(&self, url: &str, body: Vec<u8>) {
        self.files.lock().unwrap().insert(url.to_owned(), body);
    }

    pub(in crate::app) fn publish(&self, key: &SigningKey, json: &serde_json::Value) {
        let url = manifest_url(&v("0.2.0"));
        let body = json.to_string().into_bytes();
        self.serve(&format!("{url}.sig"), sign(key, &body));
        self.serve(&url, body);
    }

    fn asked(&self) -> usize {
        self.asked.lock().unwrap().len()
    }
}

impl UpdateSource for Fake {
    fn fetch(&self, url: &str, max_bytes: u64) -> Result<Vec<u8>, String> {
        self.asked.lock().unwrap().push(url.to_owned());
        let body = self.files.lock().unwrap().get(url).cloned().ok_or("HTTP 404")?;
        (body.len() as u64 <= max_bytes).then_some(body).ok_or_else(|| "too long".into())
    }

    fn download(
        &self,
        url: &str,
        destination: &Path,
        size: u64,
        cancel: &Arc<AtomicBool>,
        progress: &dyn ProgressSink,
    ) -> Result<DownloadedArtifact, DownloadError> {
        while self.hold.load(Ordering::Acquire) {
            if cancel.load(Ordering::Acquire) {
                return Err(DownloadError::Cancelled);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let body = self.files.lock().unwrap().get(url).cloned().ok_or_else(|| DownloadError::Io(io::Error::other("HTTP 404")))?;
        engine_driver_host::stream_to_file(&mut io::Cursor::new(body), destination, size, cancel, progress)
    }
}

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tablex-update-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Removed at the end of a test.
pub(in crate::app) struct Scratch(pub(in crate::app) PathBuf);

impl Scratch {
    pub(in crate::app) fn new() -> Self {
        Self(temp_dir())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Folders a test made read-only must be removable.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn the_manifest_and_its_signature_are_fetched_from_the_release_of_that_version() {
    let fake = Fake::default();
    fake.publish(&key(1), &manifest_json("0.2.0"));
    let artifact = fetch_artifact(&fake, &trust(&key(1)), &v("0.2.0"), &v("0.1.4"), AARCH64).unwrap();
    assert_eq!(artifact.size, 1234);
    assert_eq!(
        *fake.asked.lock().unwrap(),
        [
            "https://github.com/tablex-org/tablex/releases/download/v0.2.0/app-update.json",
            "https://github.com/tablex-org/tablex/releases/download/v0.2.0/app-update.json.sig",
        ]
    );
    let error = fetch_artifact(&Fake::default(), &trust(&key(1)), &v("0.2.0"), &v("0.1.4"), AARCH64).unwrap_err();
    assert!(matches!(error, ManifestError::Fetch(_)), "{error}");
}

// --- where the app runs -------------------------------------------------------------------------

pub(in crate::app) fn fake_app_layout(root: &Path, name: &str) -> PathBuf {
    let exe = root.join(name).join("Contents/MacOS/TableX");
    std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
    std::fs::write(&exe, b"x").unwrap();
    exe
}

#[test]
fn the_bundle_is_found_from_the_executable_and_dev_runs_and_debug_builds_are_not_installable() {
    let scratch = Scratch::new();
    let exe = fake_app_layout(&scratch.0, "TableX.app");
    assert_eq!(locate_bundle(Some(&exe), false), Ok(Bundle { path: scratch.0.join("TableX.app") }));
    assert_eq!(locate_bundle(Some(&exe), true), Err(Unavailable::DebugBuild));
    assert_eq!(locate_bundle(Some(Path::new("/repo/target/debug/tablex-gpui")), false), Err(Unavailable::NotBundle));
    assert_eq!(locate_bundle(Some(&fake_app_layout(&scratch.0, "Folder")), false), Err(Unavailable::NotBundle));
    assert_eq!(locate_bundle(None, false), Err(Unavailable::NotBundle));
}

#[cfg(unix)]
#[test]
fn a_folder_that_cannot_be_written_is_not_installable_and_says_so() {
    use std::os::unix::fs::PermissionsExt as _;
    let scratch = Scratch::new();
    let exe = fake_app_layout(&scratch.0, "TableX.app");
    std::fs::set_permissions(&scratch.0, std::fs::Permissions::from_mode(0o555)).unwrap();
    let result = locate_bundle(Some(&exe), false);
    // Root writes anywhere.
    if directory_writable(&scratch.0) {
        return;
    }
    assert_eq!(result, Err(Unavailable::NotWritable(scratch.0.clone())));
    let note = result.unwrap_err().note().unwrap();
    assert!(note.contains("not writable") && note.contains(scratch.0.to_str().unwrap()), "{note}");
}

#[test]
fn a_disk_image_or_a_translocated_launch_is_told_to_move_the_app() {
    for folder in ["/Volumes/TableX", "/private/var/folders/x/AppTranslocation/ABC/d"] {
        let note = Unavailable::NotWritable(PathBuf::from(folder)).note().unwrap();
        assert_eq!(note, "Move TableX to Applications to update it.", "{folder}");
    }
    let note = Unavailable::NotWritable(PathBuf::from("/Applications")).note().unwrap();
    assert!(note.contains("/Applications") && note.contains("not writable"), "{note}");
}

#[test]
fn the_plan_is_install_only_with_a_verified_manifest_and_a_bundle_and_download_otherwise() {
    let scratch = Scratch::new();
    let exe = fake_app_layout(&scratch.0, "TableX.app");
    let fake = Fake::default();
    let trust_key = trust(&key(1));
    let plan = |fake: &Fake, trust: Option<&ManifestTrust>, exe: Option<&Path>, debug: bool| {
        plan(fake, trust, exe, debug, &v("0.2.0"), "0.1.4", AARCH64)
    };

    // No manifest published: Download, with the reason for the log only.
    match plan(&fake, Some(&trust_key), Some(&exe), false) {
        Plan::Download { note: None, problem: Some(problem) } => assert!(problem.contains("not available"), "{problem}"),
        other => panic!("{other:?}"),
    }
    // Signed by another key: Download.
    fake.publish(&key(2), &manifest_json("0.2.0"));
    assert!(matches!(plan(&fake, Some(&trust_key), Some(&exe), false), Plan::Download { problem: Some(_), .. }));
    // A build that cannot verify anything: Download.
    assert!(matches!(plan(&fake, None, Some(&exe), false), Plan::Download { problem: Some(_), .. }));

    fake.publish(&key(1), &manifest_json("0.2.0"));
    match plan(&fake, Some(&trust_key), Some(&exe), false) {
        Plan::Install { artifact, bundle } => {
            assert_eq!(artifact.size, 1234);
            assert_eq!(bundle.path, scratch.0.join("TableX.app"));
        }
        other => panic!("{other:?}"),
    }

    // A debug build or a run outside a bundle never asks the network and offers Download.
    let asked = fake.asked();
    for (exe, debug) in [(Some(exe.as_path()), true), (None, false), (Some(Path::new("/repo/target/release/tablex-gpui")), false)] {
        assert_eq!(plan(&fake, Some(&trust_key), exe, debug), Plan::Download { note: None, problem: None });
    }
    assert_eq!(fake.asked(), asked);
}

// --- the unpacked bundle -------------------------------------------------------------------------

/// A thin or universal Mach-O header.
fn macho(cpus: &[u32], universal: bool) -> Vec<u8> {
    let mut bytes = Vec::new();
    if universal {
        bytes.extend_from_slice(&0xCAFE_BABE_u32.to_be_bytes());
        bytes.extend_from_slice(&(cpus.len() as u32).to_be_bytes());
        for cpu in cpus {
            bytes.extend_from_slice(&cpu.to_be_bytes());
            bytes.extend_from_slice(&[0; 16]);
        }
    } else {
        bytes.extend_from_slice(&0xFEED_FACF_u32.to_le_bytes());
        bytes.extend_from_slice(&cpus[0].to_le_bytes());
        bytes.extend_from_slice(&[0; 24]);
    }
    bytes
}

#[test]
fn the_executable_must_have_a_slice_for_this_cpu() {
    let scratch = Scratch::new();
    let (arm, intel) = (0x0100_000C, 0x0100_0007);
    let check = |name: &str, bytes: Vec<u8>, target: &str| {
        let path = scratch.0.join(name);
        std::fs::write(&path, bytes).unwrap();
        executable_runs_on(&path, target)
    };
    assert!(check("thin-arm", macho(&[arm], false), AARCH64).unwrap());
    assert!(!check("thin-arm2", macho(&[arm], false), X86_64).unwrap());
    assert!(!check("thin-intel", macho(&[intel], false), AARCH64).unwrap());
    assert!(check("fat", macho(&[intel, arm], true), AARCH64).unwrap());
    assert!(check("fat2", macho(&[intel, arm], true), X86_64).unwrap());
    assert!(!check("fat-intel", macho(&[intel], true), AARCH64).unwrap());
    assert!(check("script", b"#!/bin/sh\n".to_vec(), AARCH64).is_err());
    assert!(!check("other", macho(&[arm], false), "aarch64-unknown-linux-gnu").unwrap());
}

/// A bundle with the identifier, version and a copy of a real (universal) executable, ad-hoc signed
/// as `bundle-macos.sh` does.
#[cfg(target_os = "macos")]
pub(in crate::app) fn make_bundle(root: &Path, name: &str, identifier: &str, version: &str) -> PathBuf {
    let app = root.join(name);
    let macos = app.join("Contents/MacOS");
    std::fs::create_dir_all(&macos).unwrap();
    std::fs::copy("/usr/bin/true", macos.join("TableX")).unwrap();
    std::fs::write(
        app.join("Contents/Info.plist"),
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>{identifier}</string>
<key>CFBundleExecutable</key><string>TableX</string>
<key>CFBundleShortVersionString</key><string>{version}</string>
</dict></plist>
"#
        ),
    )
    .unwrap();
    let signed = Command::new(CODESIGN).args(["--force", "--deep", "--sign", "-"]).arg(&app).output().unwrap();
    assert!(signed.status.success(), "{}", String::from_utf8_lossy(&signed.stderr));
    app
}

#[cfg(target_os = "macos")]
pub(in crate::app) fn zip_of(app: &Path, zip: &Path) {
    let status = Command::new(DITTO).args(["-c", "-k", "--keepParent"]).arg(app).arg(zip).status().unwrap();
    assert!(status.success());
}

#[cfg(target_os = "macos")]
pub(in crate::app) fn running(root: &Path) -> Bundle {
    Bundle { path: make_bundle(root, "TableX.app", "org.tablex.TableX", "0.1.4") }
}

#[cfg(target_os = "macos")]
#[test]
fn a_bundle_of_this_app_at_the_manifest_version_verifies() {
    let scratch = Scratch::new();
    let running = running(&scratch.0);
    let other = scratch.0.join("new");
    let new = make_bundle(&other, "TableX.app", "org.tablex.TableX", "0.2.0");
    verify_bundle(&new, &running, &v("0.2.0"), AARCH64).unwrap();
    verify_bundle(&new, &running, &v("0.2.0"), X86_64).unwrap();
}

#[cfg(target_os = "macos")]
#[test]
fn a_bundle_with_another_identifier_or_version_or_cpu_or_a_broken_signature_is_refused() {
    let scratch = Scratch::new();
    let running = running(&scratch.0);
    let dir = |name: &str| scratch.0.join(name);
    let message = |result: Result<(), InstallError>| match result.unwrap_err() {
        InstallError::Bundle(message) => message,
        other => panic!("{other}"),
    };

    let wrong_id = make_bundle(&dir("a"), "TableX.app", "com.example.Other", "0.2.0");
    let text = message(verify_bundle(&wrong_id, &running, &v("0.2.0"), AARCH64));
    assert!(text.contains("com.example.Other") && text.contains("org.tablex.TableX"), "{text}");

    let wrong_version = make_bundle(&dir("b"), "TableX.app", "org.tablex.TableX", "0.9.0");
    let text = message(verify_bundle(&wrong_version, &running, &v("0.2.0"), AARCH64));
    assert!(text.contains("0.9.0") && text.contains("0.2.0"), "{text}");

    let wrong_cpu = make_bundle(&dir("c"), "TableX.app", "org.tablex.TableX", "0.2.0");
    std::fs::write(wrong_cpu.join("Contents/MacOS/TableX"), macho(&[0x0100_0007], false)).unwrap();
    let text = message(verify_bundle(&wrong_cpu, &running, &v("0.2.0"), AARCH64));
    assert!(text.contains("not built for"), "{text}");

    // Changed after it was signed.
    let tampered = make_bundle(&dir("d"), "TableX.app", "org.tablex.TableX", "0.2.0");
    std::fs::write(tampered.join("Contents/Resources-extra"), b"added").unwrap();
    let text = message(verify_bundle(&tampered, &running, &v("0.2.0"), AARCH64));
    assert!(text.contains("code signature"), "{text}");
}

#[cfg(target_os = "macos")]
#[test]
fn a_download_is_checked_unpacked_and_verified_beside_the_running_bundle() {
    let scratch = Scratch::new();
    let running = running(&scratch.0);
    let packed = scratch.0.join("packed");
    let new = make_bundle(&packed, "TableX.app", "org.tablex.TableX", "0.2.0");
    let zip = scratch.0.join("release.zip");
    zip_of(&new, &zip);
    let bytes = std::fs::read(&zip).unwrap();
    let artifact = Artifact { url: URL.into(), size: bytes.len() as u64, sha256: hex::encode(sha2::Sha256::digest(&bytes)) };
    let fake = Fake::default();
    fake.serve(URL, bytes.clone());
    let downloads = scratch.0.join("downloads");
    let (cancel, progress) = (Arc::new(AtomicBool::new(false)), Progress::default());

    let staged = download_and_stage(&fake, &artifact, &running, &v("0.2.0"), AARCH64, &downloads, &cancel, &progress).unwrap();
    assert_eq!(staged.bundle.parent().unwrap(), staged.staging_dir);
    assert_eq!(staged.staging_dir.parent().unwrap(), scratch.0, "next to the running bundle: one volume");
    assert!(staged.bundle.join("Contents/Info.plist").is_file());
    assert_eq!(std::fs::read_dir(&downloads).unwrap().count(), 0, "the zip is removed");
    assert_eq!(progress.stage(), Stage::Installing);
    assert!((progress.fraction() - 1.0).abs() < f32::EPSILON);

    // The same file offered as a different version is not installed, and nothing is left behind.
    let before = std::fs::read_dir(&scratch.0).unwrap().count();
    let error = download_and_stage(&fake, &artifact, &running, &v("0.3.0"), AARCH64, &downloads, &cancel, &progress).unwrap_err();
    assert!(matches!(error, InstallError::Bundle(_)), "{error}");
    assert_eq!(std::fs::read_dir(&scratch.0).unwrap().count(), before, "the staging directory is removed");
}

#[cfg(target_os = "macos")]
#[test]
fn an_archive_without_exactly_one_app_is_refused() {
    let scratch = Scratch::new();
    let running = running(&scratch.0);
    let empty = scratch.0.join("empty");
    std::fs::create_dir_all(empty.join("docs")).unwrap();
    std::fs::write(empty.join("docs/readme"), b"x").unwrap();
    let zip = scratch.0.join("none.zip");
    zip_of(&empty.join("docs"), &zip);
    let error = stage(&zip, &running, &v("0.2.0"), AARCH64).unwrap_err();
    assert!(matches!(error, InstallError::Bundle(_)), "{error}");
    let error = stage(&scratch.0.join("missing.zip"), &running, &v("0.2.0"), AARCH64).unwrap_err();
    assert!(matches!(error, InstallError::Unpack(_)), "{error}");
}

// --- download checks --------------------------------------------------------------------------

#[test]
fn a_download_that_is_not_the_signed_file_is_refused_and_removed() {
    let scratch = Scratch::new();
    let running = Bundle { path: scratch.0.join("TableX.app") };
    let body = b"the update".to_vec();
    let artifact = Artifact { url: URL.into(), size: body.len() as u64, sha256: hex::encode(sha2::Sha256::digest(&body)) };
    let downloads = scratch.0.join("downloads");
    let run = |served: Vec<u8>, cancel: bool| {
        let fake = Fake::default();
        fake.serve(URL, served);
        let (cancel, progress) = (Arc::new(AtomicBool::new(cancel)), Progress::default());
        download_and_stage(&fake, &artifact, &running, &v("0.2.0"), AARCH64, &downloads, &cancel, &progress)
    };

    // Same size, other bytes: the SHA-256 differs.
    let error = run(b"the UPDATE".to_vec(), false).unwrap_err();
    assert!(matches!(error, InstallError::Checksum(_)), "{error}");
    // Other size: the size cap of the download itself.
    let error = run(b"a longer update".to_vec(), false).unwrap_err();
    assert!(matches!(error, InstallError::Download(_)), "{error}");
    let error = run(b"short".to_vec(), false).unwrap_err();
    assert!(matches!(error, InstallError::Download(_)), "{error}");
    // Nothing is served.
    let error = download_and_stage(
        &Fake::default(),
        &artifact,
        &running,
        &v("0.2.0"),
        AARCH64,
        &downloads,
        &Arc::new(AtomicBool::new(false)),
        &Progress::default(),
    )
    .unwrap_err();
    assert!(matches!(error, InstallError::Download(_)), "{error}");
    // Cancelled.
    let error = run(body, true).unwrap_err();
    assert!(matches!(error, InstallError::Cancelled), "{error}");

    assert_eq!(std::fs::read_dir(&downloads).unwrap().count(), 0, "no partial or refused file stays");
    assert_eq!(std::fs::read_dir(&scratch.0).unwrap().count(), 1, "only the downloads folder: nothing was unpacked");
}

// --- the swap -----------------------------------------------------------------------------------

/// `TableX.app` holding `old` and a staged bundle holding `new`, in one folder.
fn swap_fixture(root: &Path) -> (Bundle, Staged) {
    let bundle = Bundle { path: root.join("TableX.app") };
    std::fs::create_dir_all(&bundle.path).unwrap();
    std::fs::write(bundle.path.join("version"), "old").unwrap();
    let staging_dir = root.join(format!("{STAGING_PREFIX}1"));
    std::fs::create_dir_all(staging_dir.join("TableX.app")).unwrap();
    std::fs::write(staging_dir.join("TableX.app/version"), "new").unwrap();
    (bundle, Staged { bundle: staging_dir.join("TableX.app"), staging_dir })
}

fn version_of(app: &Path) -> String {
    std::fs::read_to_string(app.join("version")).unwrap()
}

fn real_rename(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::rename(from, to)
}

#[test]
fn the_swap_moves_the_running_bundle_aside_and_the_new_one_into_place() {
    let scratch = Scratch::new();
    let (bundle, staged) = swap_fixture(&scratch.0);
    let backup = swap(&bundle, &staged, "0.1.4", &real_rename).unwrap();
    assert_eq!(backup, scratch.0.join("TableX.app.old-0.1.4"));
    assert_eq!(version_of(&bundle.path), "new");
    assert_eq!(version_of(&backup), "old");
    assert!(!staged.staging_dir.exists());
}

#[test]
fn a_stale_backup_of_the_same_version_does_not_block_the_swap() {
    let scratch = Scratch::new();
    let (bundle, staged) = swap_fixture(&scratch.0);
    let stale = scratch.0.join("TableX.app.old-0.1.4");
    std::fs::create_dir_all(stale.join("deep")).unwrap();
    swap(&bundle, &staged, "0.1.4", &real_rename).unwrap();
    assert_eq!(version_of(&stale), "old");
}

#[test]
fn a_failure_to_move_the_new_bundle_in_puts_the_old_one_back() {
    let scratch = Scratch::new();
    let (bundle, staged) = swap_fixture(&scratch.0);
    let calls = Mutex::new(0);
    let failing_second = |from: &Path, to: &Path| {
        let mut calls = calls.lock().unwrap();
        *calls += 1;
        if *calls == 2 {
            return Err(io::Error::other("disk on fire"));
        }
        std::fs::rename(from, to)
    };
    let error = swap(&bundle, &staged, "0.1.4", &failing_second).unwrap_err();
    assert!(matches!(&error, InstallError::Swap(text) if text.contains("disk on fire")), "{error}");
    assert_eq!(*calls.lock().unwrap(), 3, "aside, in (failed), back");
    assert_eq!(version_of(&bundle.path), "old", "the running app is where it was");
    assert!(!scratch.0.join("TableX.app.old-0.1.4").exists());
    assert_eq!(version_of(&staged.bundle), "new", "the staged bundle is untouched");
}

#[test]
fn when_the_old_bundle_cannot_be_put_back_the_error_names_the_backup() {
    let scratch = Scratch::new();
    let (bundle, staged) = swap_fixture(&scratch.0);
    let calls = Mutex::new(0);
    let failing = |from: &Path, to: &Path| {
        let mut calls = calls.lock().unwrap();
        *calls += 1;
        if *calls >= 2 {
            return Err(io::Error::other("nope"));
        }
        std::fs::rename(from, to)
    };
    let error = swap(&bundle, &staged, "0.1.4", &failing).unwrap_err().to_string();
    assert!(error.contains("could not be put back") && error.contains("TableX.app.old-0.1.4"), "{error}");
}

#[test]
fn a_failure_to_move_the_old_bundle_aside_changes_nothing() {
    let scratch = Scratch::new();
    let (bundle, staged) = swap_fixture(&scratch.0);
    let failing = |_: &Path, _: &Path| Err(io::Error::other("busy"));
    let error = swap(&bundle, &staged, "0.1.4", &failing).unwrap_err();
    assert!(matches!(error, InstallError::Swap(_)));
    assert_eq!(version_of(&bundle.path), "old");
    assert_eq!(version_of(&staged.bundle), "new");
}

// --- clean-up -----------------------------------------------------------------------------------

#[test]
fn a_launch_removes_old_enough_unused_backups_and_old_leftovers_and_nothing_else() {
    let scratch = Scratch::new();
    let bundle = Bundle { path: scratch.0.join("TableX.app") };
    let downloads = scratch.0.join("updates");
    for dir in [
        "TableX.app",
        "TableX.app.old-0.1.4",
        "TableX.app.old-0.1.3",
        ".TableX-update-abc",
        "Other.app",
        "Other.app.old-1.0.0",
        "notes",
    ] {
        std::fs::create_dir_all(scratch.0.join(dir)).unwrap();
    }
    std::fs::create_dir_all(&downloads).unwrap();
    std::fs::write(downloads.join("TableX-0.2.0-x.zip"), b"partial").unwrap();
    let left = |name: &str| scratch.0.join(name).exists();
    let hour = Duration::from_secs(3600);

    // Just made: a young backup, staging directory and download may belong to an install that runs.
    let now = SystemTime::now();
    clean_up(&bundle, Some(&downloads), hour, now);
    assert!(left("TableX.app.old-0.1.4") && left("TableX.app.old-0.1.3"), "a backup is kept for a while");
    assert!(left(".TableX-update-abc") && downloads.join("TableX-0.2.0-x.zip").exists());
    assert!(left("TableX.app") && left("Other.app") && left("Other.app.old-1.0.0") && left("notes"));

    // Old enough, no marker: the backups go; the staging directory and download wait for an hour.
    clean_up(&bundle, Some(&downloads), hour, now + BACKUP_MIN_AGE + Duration::from_secs(1));
    assert!(!left("TableX.app.old-0.1.4") && !left("TableX.app.old-0.1.3"));
    assert!(left(".TableX-update-abc") && downloads.join("TableX-0.2.0-x.zip").exists());

    clean_up(&bundle, Some(&downloads), hour, now + 2 * hour);
    assert!(!left(".TableX-update-abc") && !downloads.join("TableX-0.2.0-x.zip").exists(), "old ones go");
    assert!(left("TableX.app") && left("Other.app") && left("Other.app.old-1.0.0") && left("notes"));
}

#[cfg(unix)]
#[test]
fn a_backup_that_a_live_process_runs_from_is_never_removed() {
    let scratch = Scratch::new();
    let bundle = Bundle { path: scratch.0.join("TableX.app") };
    let live = scratch.0.join("TableX.app.old-0.1.4");
    let dead = scratch.0.join("TableX.app.old-0.1.3");
    for dir in [&live, &dead] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(live.join(BACKUP_MARKER), std::process::id().to_string()).unwrap();
    // The pid of a process that has exited.
    let mut child = Command::new("/usr/bin/true").spawn().unwrap();
    let gone = child.id();
    child.wait().unwrap();
    std::fs::write(dead.join(BACKUP_MARKER), gone.to_string()).unwrap();

    let much_later = SystemTime::now() + 24 * Duration::from_secs(3600);
    clean_up(&bundle, None, Duration::from_secs(3600), much_later);
    assert!(live.exists(), "this process runs from it");
    assert!(!dead.exists());
}

#[test]
fn the_swap_leaves_a_marker_naming_the_process_that_runs_from_the_backup() {
    let scratch = Scratch::new();
    let (bundle, staged) = swap_fixture(&scratch.0);
    let backup = swap(&bundle, &staged, "0.1.4", &real_rename).unwrap();
    let pid = std::fs::read_to_string(backup.join(BACKUP_MARKER)).unwrap();
    assert_eq!(pid, std::process::id().to_string());
    assert!(!bundle.path.join(BACKUP_MARKER).exists(), "not in the new bundle");
}

#[cfg(unix)]
#[test]
fn the_swap_does_not_replace_a_backup_that_a_live_process_runs_from() {
    let scratch = Scratch::new();
    let (bundle, staged) = swap_fixture(&scratch.0);
    let backup = scratch.0.join("TableX.app.old-0.1.4");
    std::fs::create_dir_all(&backup).unwrap();
    std::fs::write(backup.join(BACKUP_MARKER), std::process::id().to_string()).unwrap();
    let error = swap(&bundle, &staged, "0.1.4", &real_rename).unwrap_err();
    assert!(matches!(&error, InstallError::Swap(text) if text.contains("still running")), "{error}");
    assert!(backup.join(BACKUP_MARKER).exists());
    assert_eq!(version_of(&bundle.path), "old");
}

#[test]
fn the_last_step_swaps_unless_cancelled_and_never_leaves_the_staging_directory_behind() {
    let scratch = Scratch::new();
    let (bundle, staged) = swap_fixture(&scratch.0);
    let staging = staged.staging_dir.clone();
    let error = install_staged(&bundle, staged, "0.1.4", &AtomicBool::new(true)).unwrap_err();
    assert!(matches!(error, InstallError::Cancelled));
    assert!(!staging.exists(), "a cancelled install removes what it unpacked");
    assert_eq!(version_of(&bundle.path), "old");

    let (_, staged) = swap_fixture(&scratch.0);
    install_staged(&bundle, staged, "0.1.4", &AtomicBool::new(false)).unwrap();
    assert_eq!(version_of(&bundle.path), "new");

    // A failing swap (the backup name is taken by a backup a live process runs from): the staging
    // directory still goes.
    let (_, staged) = swap_fixture(&scratch.0);
    let staging = staged.staging_dir.clone();
    std::fs::create_dir_all(scratch.0.join("TableX.app.old-9.9.9")).unwrap();
    std::fs::write(scratch.0.join("TableX.app.old-9.9.9").join(BACKUP_MARKER), std::process::id().to_string()).unwrap();
    let error = install_staged(&bundle, staged, "9.9.9", &AtomicBool::new(false)).unwrap_err();
    assert!(matches!(error, InstallError::Swap(_)), "{error}");
    assert!(!staging.exists());
}

// --- the restart ----------------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn the_relaunch_waits_for_the_process_to_exit_then_opens_the_bundle() {
    use std::os::unix::fs::PermissionsExt as _;
    let scratch = Scratch::new();
    let marker = scratch.0.join("opened");
    // Stands in for `/usr/bin/open`: records its arguments.
    let open = scratch.0.join("fake-open");
    std::fs::write(&open, format!("#!/bin/sh\necho \"$@\" > '{}'\n", marker.display())).unwrap();
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
    // A process that is still alive for a moment.
    let mut sleeper = Command::new("/bin/sleep").arg("1").spawn().unwrap();
    relaunch_with(Path::new("/tmp/Some App.app"), sleeper.id(), open.to_str().unwrap()).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert!(!marker.exists(), "not while the old process is alive");
    sleeper.wait().unwrap();
    for _ in 0..100 {
        if marker.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), "-n /tmp/Some App.app");
}

// --- the real release ---------------------------------------------------------------------------

/// Reads the manifest of the latest published release with the keys of this build. Releases made
/// before the manifest existed have none: the test then says so and passes.
/// `cargo test -p tablex-gpui live_latest_release_manifest -- --ignored --nocapture`
#[test]
#[ignore = "needs the network and a release that publishes app-update.json"]
fn live_latest_release_manifest_verifies_with_the_pinned_keys() {
    use super::super::check::{parse_latest, ReleaseFetcher as _};
    let body = super::super::check::HttpReleaseFetcher.fetch().expect("the latest release");
    let release = parse_latest(&body).unwrap().expect("an app release");
    let trust = crate::drivers::manifest_trust(TrustPolicy::Strict).expect("pinned keys");
    match fetch_artifact(&HttpSource, &trust, &release.version, &v("0.0.0"), current_target()) {
        Ok(artifact) => println!("{}: {} bytes, sha256 {}", release.version, artifact.size, artifact.sha256),
        Err(error @ ManifestError::Fetch(_)) => println!("SKIPPED: {} has no app-update.json yet ({error})", release.version),
        Err(error) => panic!("{} has a manifest this build refuses: {error}", release.version),
    }
}
