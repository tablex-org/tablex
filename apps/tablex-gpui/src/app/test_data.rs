//! Test-only: keeps every `AppRoot` a test builds off the user's TableX data.
//!
//! Real mode (no UI scenario) reads the connection store, saved SQL, the assistant's conversations
//! and the other files under the app data dir, and a scenario app whose test turns the fixture
//! service off (`scenario_ui.service = None`) reaches the connection store too. The stores open
//! once per process, so `TABLEX_DATA_DIR` must name a temporary dir before the first of those
//! reads: every `AppRoot` construction calls `isolate`, and a real-mode one also has to be asked
//! for with `opt_in_real_mode` (`AppRoot::new_isolated`), or `assert_real_mode_isolated` fails it.

use engine_storage::paths::{data_dir_override, DATA_DIR_ENV};
use std::cell::Cell;
use std::path::PathBuf;
use std::sync::OnceLock;

static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

thread_local! {
    static REAL_MODE_ASKED_FOR: Cell<bool> = const { Cell::new(false) };
}

/// Points `TABLEX_DATA_DIR` at a per-process temporary dir (first call only).
pub(super) fn isolate() {
    DATA_DIR.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("tablex-gpui-test-data-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create the temporary data dir");
        std::env::set_var(DATA_DIR_ENV, &dir);
        dir
    });
}

/// The next real-mode `AppRoot` built on this thread is deliberate (see `assert_real_mode_isolated`).
pub(super) fn opt_in_real_mode() {
    isolate();
    REAL_MODE_ASKED_FOR.set(true);
}

/// Panics unless the real-mode `AppRoot` being built was asked for with `opt_in_real_mode` and
/// `TABLEX_DATA_DIR` still names the temporary dir.
pub(super) fn assert_real_mode_isolated() {
    let asked_for = REAL_MODE_ASKED_FOR.replace(false);
    assert!(
        asked_for && DATA_DIR.get().is_some() && data_dir_override().as_ref() == DATA_DIR.get(),
        "a test built a real-mode AppRoot on the user's TableX data: use \
         AppRoot::new_for_test (fixture scenario) or AppRoot::new_isolated"
    );
}
