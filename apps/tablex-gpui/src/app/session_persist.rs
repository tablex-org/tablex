//! Restores and saves the GPUI workspace session (`crate::gpui_session`).
//!
//! Saving is event driven: `render` notices a changed `WorkspaceState`
//! revision and starts one debounce task that waits for 500ms of quiet before
//! snapshotting; the file write itself runs on the background executor. The
//! pending task is the only timer and exists only while a change is unsaved.

use super::*;
use crate::gpui_session::{self, LoadOutcome};
use std::path::PathBuf;

const SAVE_DEBOUNCE: Duration = Duration::from_millis(500);
/// Scenario/test runs only persist to an injected temp-dir file.
const SCENARIO_PATH_ENV: &str = "TABLEX_GPUI_SESSION_PATH";

#[derive(Default)]
pub(super) struct SessionPersist {
    /// Where the session lives; `None` disables restore and save entirely.
    pub(super) path: Option<PathBuf>,
    /// Workspace revision already on disk (or deliberately not saved).
    saved_revision: u64,
    /// Orders and serializes every file write (background and quit-time) and
    /// remembers what is really on disk.
    pub(super) writer: std::sync::Arc<gpui_session::SessionWriter>,
    pending: Option<Task<()>>,
    /// Number of file writes dispatched (tests assert debouncing with it).
    pub(super) writes: usize,
    _quit: Option<gpui::Subscription>,
}

/// Real runs use the Application Support file. Scenario runs never touch it:
/// they persist only when the launcher injects an absolute temp-dir path.
fn resolve_path(scenario_enabled: bool) -> Option<PathBuf> {
    if scenario_enabled {
        std::env::var(SCENARIO_PATH_ENV)
            .ok()
            .and_then(|raw| sidebar::scenario_prefs_path_from(&raw))
    } else if cfg!(test) {
        // Unit tests that build a real (non-scenario) AppRoot must never read
        // or write the user's Application Support session.
        None
    } else {
        gpui_session::default_path()
    }
}

fn is_data_tab(tab: &Tab) -> bool {
    tab.kind == TabKind::Query
        && (tab.query_mode.is_table_data()
            || tab.query_mode.is_mongo_find()
            || tab.query_mode.is_redis_key())
}

impl AppRoot {
    /// Resolves the session path, restores tabs (GPUI file first, else the
    /// read-only Swift import) and arms the quit-time flush.
    pub(super) fn init_workspace_session(
        &mut self,
        scenario_enabled: bool,
        cx: &mut Context<Self>,
    ) {
        self.shell.session.path = resolve_path(scenario_enabled);
        self.restore_workspace_session(scenario_enabled, restore_swift_sql_tabs, cx);
        self.shell.session.saved_revision = self.workspace.revision();
        if self.shell.session.path.is_some() {
            self.shell.session._quit = Some(cx.on_app_quit(|this, _| {
                this.flush_session_blocking();
                async {}
            }));
        }
    }

    /// `swift_import` is the read-only Swift session fallback (injectable so
    /// tests never read the real `~/Library`).
    pub(super) fn restore_workspace_session(
        &mut self,
        scenario_enabled: bool,
        swift_import: impl FnOnce(&mut WorkspaceState, &[SavedConnection]),
        cx: &mut Context<Self>,
    ) {
        // "Restore tabs on start" (Settings › Navigation).
        let restore = self.settings.navigation.restore_tabs;
        if restore == crate::settings::RestoreTabs::None {
            return;
        }
        if let Some(path) = self.shell.session.path.clone() {
            match gpui_session::load(&path) {
                LoadOutcome::Loaded(session) => {
                    self.apply_session(&session, scenario_enabled, cx);
                    if restore == crate::settings::RestoreTabs::Pinned {
                        let unpinned: Vec<u64> = self
                            .workspace
                            .tabs()
                            .iter()
                            .filter(|tab| !tab.is_pinned && tab.kind != TabKind::Welcome)
                            .map(|tab| tab.id)
                            .collect();
                        for id in unpinned {
                            self.close_tab(id, cx);
                        }
                        if self.workspace.tabs().is_empty() {
                            self.workspace.open_welcome_tab();
                        }
                    }
                    return;
                }
                LoadOutcome::Unusable(message) => {
                    gpui_session::quarantine(&path);
                    self.report_warning(Area::App, None, &message);
                    self.sidebar.notice = Some(message);
                }
                LoadOutcome::Missing => {}
            }
        }
        if !scenario_enabled {
            swift_import(&mut self.workspace, &self.saved_connections);
        }
    }

    fn apply_session(
        &mut self,
        session: &gpui_session::GpuiSession,
        scenario_enabled: bool,
        cx: &mut Context<Self>,
    ) {
        let connections = &self.saved_connections;
        self.workspace.restore_snapshot(session, |tab| {
            tab.connection_id.is_empty()
                || connections
                    .iter()
                    .any(|saved| saved.matches_id(&tab.connection_id))
        });
        if self.workspace.tabs().is_empty() && !session.tabs.is_empty() {
            self.workspace.open_welcome_tab();
        }
        // Only the active data tab loads at startup; the others wait for the
        // user (their grids stay empty until activated and refreshed).
        if !scenario_enabled && self.workspace.active_tab().is_some_and(is_data_tab) {
            self.run_active_tab_query(cx);
        }
        // A source tab shows what the server has now: fetch it when it is the active tab.
        if let Some(tab_id) = self
            .workspace
            .active_tab()
            .filter(|tab| tab.kind == TabKind::Source)
            .map(|tab| tab.id)
        {
            self.ensure_source_loaded(tab_id, cx);
        }
    }

    /// Called every frame; costs one comparison unless something changed.
    pub(super) fn schedule_session_save(&mut self, cx: &mut Context<Self>) {
        let session = &self.shell.session;
        if session.path.is_none()
            || session.pending.is_some()
            || session.saved_revision == self.workspace.revision()
        {
            return;
        }
        self.shell.session.pending = Some(cx.spawn(async move |this, cx| {
            loop {
                let Ok(seen) = this.update(cx, |this, _| this.workspace.revision()) else {
                    return;
                };
                cx.background_executor().timer(SAVE_DEBOUNCE).await;
                match this.update(cx, |this, cx| {
                    let quiet = this.workspace.revision() == seen;
                    if quiet {
                        this.flush_session(cx);
                    }
                    quiet
                }) {
                    Ok(true) => break,
                    Ok(false) => continue,
                    Err(_) => return,
                }
            }
            let _ = this.update(cx, |this, _| this.shell.session.pending = None);
        }));
    }

    /// Snapshots now and writes on the background executor (skipped when the
    /// bytes equal what is already on disk).
    pub(super) fn flush_session(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.shell.session.path.clone() else {
            return;
        };
        self.shell.session.saved_revision = self.workspace.revision();
        let bytes = match gpui_session::encode(&self.workspace.snapshot()) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.report_error(Area::App, None, format!("Could not save the workspace: {error}"));
                self.sidebar.notice = Some(format!("Could not save the workspace: {error}"));
                return;
            }
        };
        // `None`: these exact bytes were already submitted (and did not fail).
        let Some(ticket) = self.shell.session.writer.submit(&bytes) else {
            return;
        };
        let writer = self.shell.session.writer.clone();
        self.shell.session.writes += 1;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { writer.write(&path, ticket, &bytes) })
                .await;
            if let Err(error) = result {
                let _ = this.update(cx, |this, cx| {
                    this.report_error(Area::App, None, format!("Could not save the workspace: {error}"));
                    this.sidebar.notice = Some(format!("Could not save the workspace: {error}"));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Quit-time flush: synchronous so the last (<500ms old) change survives.
    /// It waits for any in-flight background write, then rewrites unless the
    /// file already holds exactly the current snapshot; queued older writes
    /// become no-ops, so they can never clobber it afterwards.
    pub(super) fn flush_session_blocking(&mut self) {
        let Some(path) = self.shell.session.path.clone() else {
            return;
        };
        let Ok(bytes) = gpui_session::encode(&self.workspace.snapshot()) else {
            return;
        };
        let _ = self.shell.session.writer.write_now(&path, &bytes);
    }
}
