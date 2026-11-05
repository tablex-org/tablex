#[cfg(test)]
mod tests {
    use super::*;

    struct TempStore(std::path::PathBuf);

    impl TempStore {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("tablex-snowflake-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempStore {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn reservations_persist_across_instances_and_worker_ids() {
        let dir = TempStore::new();
        let now = SNOWFLAKE_EPOCH_MS + 1_234;
        let first = SnowflakeStore::reserve(&dir.0, 17, 3, now).unwrap();
        let second = SnowflakeStore::reserve(&dir.0, 17, 3, now).unwrap();
        let other = SnowflakeStore::reserve(&dir.0, 18, 1, now).unwrap();
        assert!(first.iter().chain(&second).all(|value| *value > 0));
        assert!(first
            .windows(2)
            .chain(second.windows(2))
            .all(|pair| pair[0] < pair[1]));
        assert!(first[2] < second[0]);
        assert_ne!(first[0], other[0]);
        assert_eq!((first[0] >> 12) & 1023, 17);
    }

    #[test]
    fn concurrent_reservations_share_an_exclusive_file_lock() {
        let dir = TempStore::new();
        let now = SNOWFLAKE_EPOCH_MS + 6_000;
        let (a, b) = std::thread::scope(|scope| {
            let a = scope.spawn(|| SnowflakeStore::reserve(&dir.0, 42, 200, now));
            let b = scope.spawn(|| SnowflakeStore::reserve(&dir.0, 42, 200, now));
            (a.join().unwrap().unwrap(), b.join().unwrap().unwrap())
        });
        let mut all = a.into_iter().chain(b).collect::<Vec<_>>();
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), 400);
    }

    #[test]
    fn bounded_clock_rollback_and_sequence_exhaustion_never_reuse_ids() {
        let dir = TempStore::new();
        let now = SNOWFLAKE_EPOCH_MS + 20_000;
        let first = SnowflakeStore::reserve(&dir.0, 0, 4096, now).unwrap();
        let next = SnowflakeStore::reserve(&dir.0, 0, 1, now).unwrap();
        assert!(first[4095] < next[0]);
        let rollback = SnowflakeStore::reserve(&dir.0, 0, 1, now - 1).unwrap();
        assert!(next[0] < rollback[0]);
        assert!(SnowflakeStore::reserve(&dir.0, 0, 1, now - MAX_CLOCK_SKEW_MS - 2).is_err());
    }

    #[test]
    fn corrupted_state_and_invalid_worker_fail_closed() {
        let dir = TempStore::new();
        let now = SNOWFLAKE_EPOCH_MS + 10;
        assert!(SnowflakeStore::reserve(&dir.0, 1024, 1, now).is_err());
        let previous = SnowflakeStore::reserve(&dir.0, 11, 2, now).unwrap();
        std::fs::write(dir.0.join("worker-11.state"), b"bad").unwrap();
        assert!(SnowflakeStore::reserve(&dir.0, 11, 1, now).is_err());
        assert_eq!(previous.len(), 2);
    }

    #[test]
    fn truncated_but_parseable_legacy_state_is_rejected() {
        let dir = TempStore::new();
        let now = SNOWFLAKE_EPOCH_MS + 10_000;
        SnowflakeStore::reserve(&dir.0, 11, 2, now).unwrap();
        let path = dir.0.join("worker-11.state");
        std::fs::write(&path, b"v1 1 0").unwrap();
        assert!(SnowflakeStore::reserve(&dir.0, 11, 1, now).is_err());
        std::fs::write(&path, b"v1 1 0 \n").unwrap();
        assert!(SnowflakeStore::reserve(&dir.0, 11, 1, now).is_err());
    }

    #[test]
    fn worker_configuration_is_explicit_and_corruption_fails_closed() {
        let dir = TempStore::new();
        assert_eq!(SnowflakeStore::read_worker(&dir.0).unwrap(), None);
        assert!(SnowflakeStore::configure_worker(&dir.0, 1024).is_err());
        SnowflakeStore::configure_worker(&dir.0, 17).unwrap();
        assert_eq!(SnowflakeStore::read_worker(&dir.0).unwrap(), Some(17));
        SnowflakeStore::configure_worker(&dir.0, 18).unwrap();
        assert_eq!(SnowflakeStore::read_worker(&dir.0).unwrap(), Some(18));
        std::fs::write(dir.0.join("worker.json"), b"broken").unwrap();
        assert!(SnowflakeStore::read_worker(&dir.0).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn worker_state_and_configuration_reject_symlink_targets() {
        let dir = TempStore::new();
        let external = TempStore::new();
        let target = external.0.join("outside.json");
        std::fs::write(&target, b"{\"version\":1,\"worker_id\":17}").unwrap();
        std::os::unix::fs::symlink(&target, dir.0.join("worker.json")).unwrap();
        assert!(SnowflakeStore::read_worker(&dir.0).is_err());
        std::os::unix::fs::symlink(&target, dir.0.join("worker-17.state")).unwrap();
        assert!(SnowflakeStore::reserve(&dir.0, 17, 1, SNOWFLAKE_EPOCH_MS + 100).is_err());
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"{\"version\":1,\"worker_id\":17}"
        );
    }

    #[test]
    fn subprocess_entry_for_exclusive_state_test() {
        let Ok(dir) = std::env::var("TABLEX_SNOWFLAKE_TEST_DIR") else {
            return;
        };
        let values =
            SnowflakeStore::reserve(Path::new(&dir), 23, 200, SNOWFLAKE_EPOCH_MS + 8_000).unwrap();
        let output = Path::new(&dir).join(format!("child-{}.json", std::process::id()));
        std::fs::write(output, serde_json::to_vec(&values).unwrap()).unwrap();
    }

    #[test]
    fn separate_processes_reuse_one_durable_highwater_mark() {
        let dir = TempStore::new();
        let binary = std::env::current_exe().unwrap();
        std::thread::scope(|scope| {
            let run = || {
                std::process::Command::new(&binary)
                    .args([
                        "--exact",
                        "app::results::snowflake::tests::subprocess_entry_for_exclusive_state_test",
                    ])
                    .env("TABLEX_SNOWFLAKE_TEST_DIR", &dir.0)
                    .output()
                    .unwrap()
            };
            let a = scope.spawn(run);
            let b = scope.spawn(run);
            assert!(a.join().unwrap().status.success());
            assert!(b.join().unwrap().status.success());
        });
        let mut all = Vec::new();
        for file in std::fs::read_dir(&dir.0).unwrap() {
            let file = file.unwrap();
            if file.file_name().to_string_lossy().starts_with("child-") {
                all.extend(
                    serde_json::from_slice::<Vec<u64>>(&std::fs::read(file.path()).unwrap())
                        .unwrap(),
                );
            }
        }
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), 400);
    }

    #[test]
    fn failed_publication_keeps_previous_highwater_and_releases_lock() {
        for failure in [
            PublishStep::PartialWrite,
            PublishStep::FileSync,
            PublishStep::Rename,
            PublishStep::DirectorySync,
        ] {
            let dir = TempStore::new();
            let now = SNOWFLAKE_EPOCH_MS + 1_000;
            let first = SnowflakeStore::reserve(&dir.0, 19, 2, now).unwrap();
            let path = dir.0.join("worker-19.state");
            let previous = std::fs::read(&path).unwrap();
            assert!(
                SnowflakeStore::reserve_with_hook(&dir.0, 19, 2, now, |step| {
                    if step == failure {
                        Err("injected publication failure".into())
                    } else {
                        Ok(())
                    }
                })
                .is_err()
            );
            if failure != PublishStep::DirectorySync {
                assert_eq!(std::fs::read(&path).unwrap(), previous);
            }
            let next = SnowflakeStore::reserve(&dir.0, 19, 2, now).unwrap();
            assert!(first[1] < next[0]);
        }
    }

    #[test]
    fn subprocess_exit_during_partial_write_preserves_previous_state() {
        let Ok(dir) = std::env::var("TABLEX_SNOWFLAKE_ABORT_DIR") else {
            return;
        };
        let _ = SnowflakeStore::reserve_with_hook(
            Path::new(&dir),
            21,
            2,
            SNOWFLAKE_EPOCH_MS + 2_000,
            |step| {
                if step == PublishStep::PartialWrite {
                    std::process::exit(11)
                }
                Ok(())
            },
        );
        panic!("child did not exit during publication");
    }

    #[test]
    fn interrupted_process_cannot_lower_highwater_for_next_instance() {
        let dir = TempStore::new();
        let now = SNOWFLAKE_EPOCH_MS + 2_000;
        let first = SnowflakeStore::reserve(&dir.0, 21, 2, now).unwrap();
        let state = std::fs::read(dir.0.join("worker-21.state")).unwrap();
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "app::results::snowflake::tests::subprocess_exit_during_partial_write_preserves_previous_state"])
            .env("TABLEX_SNOWFLAKE_ABORT_DIR", &dir.0)
            .output().unwrap();
        assert_eq!(child.status.code(), Some(11));
        assert_eq!(std::fs::read(dir.0.join("worker-21.state")).unwrap(), state);
        let next = SnowflakeStore::reserve(&dir.0, 21, 2, now).unwrap();
        assert!(first[1] < next[0]);
    }
}
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

/// Owner-only state: on Unix the files are created 0600 and the directory set to 0700, and a file
/// that the group or others can reach is refused. Windows has no mode bits; a file there keeps the
/// ACL it inherits from the per-user configuration directory.
fn owner_only(options: &mut OpenOptions) -> &mut OpenOptions {
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(options, 0o600);
    options
}

fn restrict_directory(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

fn reachable_by_others(metadata: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o077 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        false
    }
}

pub(in crate::app) const SNOWFLAKE_EPOCH_MS: u64 = 1_704_067_200_000;
pub(in crate::app) const MAX_CLOCK_SKEW_MS: u64 = 5_000;
const MAX_BATCH: usize = 16_384;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PublishStep {
    PartialWrite,
    FileSync,
    Rename,
    DirectorySync,
}

fn publish_state(
    dir: &Path,
    path: &Path,
    state: &str,
    mut check: impl FnMut(PublishStep) -> Result<(), String>,
) -> Result<(), String> {
    let temporary = dir.join(format!("state-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<(), String> {
        let mut file = owner_only(OpenOptions::new().write(true).create_new(true))
            .open(&temporary)
            .map_err(|error| error.to_string())?;
        let bytes = state.as_bytes();
        let halfway = bytes.len() / 2;
        file.write_all(&bytes[..halfway])
            .map_err(|error| error.to_string())?;
        check(PublishStep::PartialWrite)?;
        file.write_all(&bytes[halfway..])
            .map_err(|error| error.to_string())?;
        check(PublishStep::FileSync)?;
        file.sync_all().map_err(|error| error.to_string())?;
        check(PublishStep::Rename)?;
        fs::rename(&temporary, path).map_err(|error| error.to_string())?;
        check(PublishStep::DirectorySync)?;
        fs::File::open(dir)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| error.to_string())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[derive(serde::Serialize, serde::Deserialize)]
struct WorkerConfig {
    version: u8,
    worker_id: u16,
}

pub(in crate::app) struct SnowflakeStore;

impl SnowflakeStore {
    pub(in crate::app) fn default_directory() -> Result<std::path::PathBuf, String> {
        dirs::config_dir()
            .map(|dir| dir.join("TableX/snowflake"))
            .ok_or("Application configuration directory is unavailable".into())
    }

    fn prepare_directory(dir: &Path) -> Result<(), String> {
        fs::create_dir_all(dir).map_err(|error| error.to_string())?;
        if !fs::symlink_metadata(dir)
            .map_err(|error| error.to_string())?
            .file_type()
            .is_dir()
        {
            return Err("Snowflake state directory must not be a symlink".into());
        }
        restrict_directory(dir).map_err(|error| error.to_string())
    }

    pub(in crate::app) fn read_worker(dir: &Path) -> Result<Option<u16>, String> {
        let path = dir.join("worker.json");
        match fs::symlink_metadata(&path) {
            Ok(metadata) if !metadata.file_type().is_file() || reachable_by_others(&metadata) => {
                return Err(
                    "Snowflake worker configuration permissions or file type are unsafe".into(),
                );
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.to_string()),
        }
        match fs::read_to_string(path) {
            Ok(text) => {
                let config: WorkerConfig = serde_json::from_str(&text)
                    .map_err(|_| "Snowflake worker configuration is corrupted".to_string())?;
                if config.version != 1 || config.worker_id > 1023 {
                    return Err("Snowflake worker configuration is invalid".into());
                }
                Ok(Some(config.worker_id))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }

    pub(in crate::app) fn configure_worker(dir: &Path, worker: u16) -> Result<(), String> {
        if worker > 1023 {
            return Err("Worker ID must be between 0 and 1023".into());
        }
        Self::prepare_directory(dir)?;
        let temporary = dir.join(format!("worker-{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| -> Result<(), String> {
            let mut file = owner_only(OpenOptions::new().write(true).create_new(true))
                .open(&temporary)
                .map_err(|error| error.to_string())?;
            let text = serde_json::to_vec(&WorkerConfig {
                version: 1,
                worker_id: worker,
            })
            .map_err(|error| error.to_string())?;
            file.write_all(&text)
                .and_then(|_| file.sync_all())
                .map_err(|error| error.to_string())?;
            fs::rename(&temporary, dir.join("worker.json")).map_err(|error| error.to_string())?;
            fs::File::open(dir)
                .and_then(|directory| directory.sync_all())
                .map_err(|error| error.to_string())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    pub(in crate::app) fn reserve(
        dir: &Path,
        worker: u16,
        count: usize,
        now_ms: u64,
    ) -> Result<Vec<u64>, String> {
        Self::reserve_with_hook(dir, worker, count, now_ms, |_| Ok(()))
    }

    fn reserve_with_hook(
        dir: &Path,
        worker: u16,
        count: usize,
        now_ms: u64,
        check: impl FnMut(PublishStep) -> Result<(), String>,
    ) -> Result<Vec<u64>, String> {
        if worker > 1023 {
            return Err("Worker ID must be between 0 and 1023".into());
        }
        if count > MAX_BATCH {
            return Err("Snowflake batch exceeds 16384 IDs".into());
        }
        if count == 0 {
            return Ok(Vec::new());
        }
        let observed = now_ms
            .checked_sub(SNOWFLAKE_EPOCH_MS)
            .filter(|elapsed| *elapsed > 0 && *elapsed < (1 << 41))
            .ok_or("Clock is outside the Snowflake 2024–2093 epoch".to_string())?;
        Self::prepare_directory(dir)?;
        let lock_path = dir.join(format!("worker-{worker}.lock"));
        if let Ok(metadata) = fs::symlink_metadata(&lock_path) {
            if !metadata.file_type().is_file() {
                return Err("Snowflake lock must be a regular file".into());
            }
        }
        let lock_file = owner_only(
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false),
        )
        .open(&lock_path)
        .map_err(|error| error.to_string())?;
        if reachable_by_others(&lock_file.metadata().map_err(|error| error.to_string())?) {
            return Err("Snowflake lock permissions are too broad".into());
        }
        lock_file.lock().map_err(|error| error.to_string())?;
        let path = dir.join(format!("worker-{worker}.state"));
        if let Ok(metadata) = fs::symlink_metadata(&path) {
            if !metadata.file_type().is_file() {
                return Err("Snowflake state must be a regular file".into());
            }
        }
        let mut state = String::new();
        match OpenOptions::new().read(true).open(&path) {
            Ok(mut file) => {
                let metadata = file.metadata().map_err(|error| error.to_string())?;
                if !metadata.is_file() || reachable_by_others(&metadata) {
                    return Err("Snowflake state file type or permissions are unsafe".into());
                }
                if metadata.len() > 64 {
                    return Err("Snowflake state is corrupted".into());
                }
                file.read_to_string(&mut state)
                    .map_err(|error| error.to_string())?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                state.push_str("v1 0 0\n")
            }
            Err(error) => return Err(error.to_string()),
        }
        let mut parts = state.split_whitespace();
        if parts.next() != Some("v1") {
            return Err("Snowflake state is corrupted".into());
        }
        let last_ms = parts
            .next()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value < (1 << 41))
            .ok_or("Snowflake state is corrupted")?;
        let last_seq = parts
            .next()
            .and_then(|value| value.parse::<u16>().ok())
            .filter(|value| *value < 4096)
            .ok_or("Snowflake state is corrupted")?;
        if parts.next().is_some() || state != format!("v1 {last_ms} {last_seq}\n") {
            return Err("Snowflake state is corrupted".into());
        }
        if last_ms > observed.saturating_add(MAX_CLOCK_SKEW_MS) {
            return Err(
                "Clock moved backwards more than 5 seconds; Snowflake IDs were not generated"
                    .into(),
            );
        }
        let mut ms = observed.max(last_ms);
        let mut seq = if ms == last_ms {
            u32::from(last_seq) + 1
        } else {
            0
        };
        let mut values = Vec::with_capacity(count);
        for _ in 0..count {
            if seq >= 4096 {
                ms = ms.checked_add(1).ok_or("Snowflake timestamp overflow")?;
                seq = 0;
            }
            if ms >= 1 << 41 || ms > observed.saturating_add(MAX_CLOCK_SKEW_MS) {
                return Err("Snowflake logical clock exceeds the 5-second future limit".into());
            }
            let id = (ms << 22) | (u64::from(worker) << 12) | u64::from(seq);
            if id == 0 || id > i64::MAX as u64 {
                return Err("Snowflake ID overflow".into());
            }
            values.push(id);
            seq += 1;
        }
        publish_state(dir, &path, &format!("v1 {ms} {}\n", seq - 1), check)?;
        lock_file.unlock().map_err(|error| error.to_string())?;
        Ok(values)
    }
}
