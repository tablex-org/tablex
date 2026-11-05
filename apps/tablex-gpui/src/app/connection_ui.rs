use super::navigator_ui::SidebarGlyph;
use super::tunnel_ui;
use super::*;
use crate::credentials::{CredentialStore, SecretKind, SystemStore};
use engine_connect::connect::Connection;
use engine_connect::records::RuntimeUri;

#[derive(Default)]
pub(super) struct ConnectionUiState {
    pub(super) test_generation: u64,
    pub(super) test_in_flight: std::rc::Rc<std::cell::Cell<bool>>,
    /// Table-data / Mongo find tabs that were inactive when their connection
    /// came back: they re-run their current query when next activated.
    pub(super) rerun_on_activate: HashSet<u64>,
    /// Inputs of the most recently opened dialog (tests inspect field state).
    #[cfg(test)]
    pub(super) last_inputs: Option<ConnectionDialogInputs>,
}

/// A driver session that is open, with the SSH tunnel it connected through: what the
/// [`Connector`] produced, with the session shared the way the app's tabs use it.
struct Connected {
    handle: Arc<engine_driver_host::DriverHandle>,
    session: Arc<engine_driver_host::SharedDriverSession>,
    connection_id: u64,
    /// The SQL dialect the driver reported for this connection when it
    /// connected (`None`: the driver's static capabilities apply).
    dialect: Option<engine_driver_api::SqlDialectInfo>,
    /// The server version in MySQL's numeric form (80411, 110809), when the driver reported it.
    server_version: Option<u32>,
    /// The URI as the user wrote it (the tunnel rewrites a copy).
    uri: RuntimeUri,
    tunnel: Option<tunnel_ui::LiveTunnel>,
}

/// The capabilities of one connection: its driver's, with the SQL dialect the
/// connect reply reported for THIS connection in place of the static one (the
/// MySQL family reports the session's real backslash-escape mode).
pub(super) fn connection_capabilities(
    driver: &engine_driver_api::DriverCapabilities,
    dialect: Option<engine_driver_api::SqlDialectInfo>,
) -> engine_driver_api::DriverCapabilities {
    let mut capabilities = driver.clone();
    if dialect.is_some() {
        capabilities.sql_dialect = dialect;
    }
    capabilities
}

impl From<Connection> for Connected {
    fn from(connection: Connection) -> Self {
        let engine_connect::connect::OpenedConnection {
            session,
            tunnel,
            handle,
            connection_id,
            uri,
        } = connection.into_parts();
        Self {
            handle,
            dialect: session.connection_dialect().cloned(),
            server_version: session.server_version(),
            session: Arc::new(engine_driver_host::SharedDriverSession::new(session)),
            connection_id,
            uri,
            tunnel,
        }
    }
}

/// Inline result of the dialog's Test button: green "Connected in N ms" or
/// the (possibly long) server error, truncated with the full text in a tooltip.
fn test_status_label(status: &crate::connection_dialog::TestStatus) -> AnyElement {
    use crate::connection_dialog::TestStatus;
    let (text, color): (String, gpui::Rgba) = match status {
        TestStatus::Idle => return div().flex_1().into_any_element(),
        TestStatus::Running => ("Testing connection...".into(), theme::text_muted()),
        TestStatus::Success { millis } => (format!("Connected in {millis} ms"), theme::success()),
        TestStatus::Failure(message) => (message.clone(), theme::danger()),
    };
    let full = text.clone();
    div()
        .id("connection-test-status")
        .debug_selector(|| "connection-test-status".into())
        .flex_1()
        .min_w(px(0.0))
        .truncate()
        .text_size(px(12.0))
        .text_color(color)
        .tooltip(move |window, cx| {
            gpui_component::tooltip::Tooltip::new(full.clone()).build(window, cx)
        })
        .child(text)
        .into_any_element()
}

pub(super) fn current_time() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0)
}

pub(super) fn connection_dialog_width(viewport_width: gpui::Pixels, picker: bool) -> gpui::Pixels {
    let preferred: f32 = if picker { 880.0 } else { 560.0 };
    let available = (f32::from(viewport_width) - 32.0).max(1.0);
    px(preferred.min(available))
}

pub(super) fn connection_dialog_height(viewport_height: gpui::Pixels) -> gpui::Pixels {
    let available = (f32::from(viewport_height) * 0.9 - 120.0).max(0.0);
    px(600.0_f32.min(available))
}

pub(super) fn runtime_uri_for_form(
    form: &ConnectionFormState,
    original_record: Option<&engine_storage::connection::SavedConnectionRecord>,
) -> Result<RuntimeUri, String> {
    if form.kind != ConnectionKind::Sqlite
        && form.password.is_empty()
        && form
            .password_from_uri().is_none_or(|password| password.is_empty())
        && form.save_password
    {
        if let Some(record) = original_record {
            if record.save_password
                && crate::connections::persisted_uris_match(
                    form.kind,
                    &record.uri,
                    &form.persisted_uri()?,
                )
            {
                return crate::sessions::engine_uri(&SavedConnection::from_record(record));
            }
        }
    }
    form.runtime_uri().map(RuntimeUri::new)
}

pub(super) fn connection_state_card(
    title: impl Into<String>,
    description: impl Into<String>,
    action: Option<AnyElement>,
) -> Div {
    connection_state_card_with(None, title, description, action.into_iter().collect())
}

/// [`connection_state_card`] with an optional leading icon and any number of
/// stacked actions (the result-area placeholder offers Reconnect and Edit).
fn connection_state_card_with(
    icon: Option<Icon>,
    title: impl Into<String>,
    description: impl Into<String>,
    actions: Vec<AnyElement>,
) -> Div {
    let mut card = div()
        .debug_selector(|| "connection-state-card".into())
        .w(px(420.0))
        .max_w_full()
        .ml_auto()
        .mr_auto()
        .p(px(20.0))
        .flex()
        .flex_col()
        .gap(px(6.0))
        .bg(theme::surface())
        .border_1()
        .border_color(theme::border())
        .children(icon.map(|icon| icon.size(px(28.0)).text_color(theme::text_muted())))
        .child(
            div()
                .text_size(px(15.0))
                .text_color(theme::text())
                .child(title.into()),
        )
        .child(
            div()
                .text_size(px(12.0))
                .text_color(theme::text_muted())
                .child(description.into()),
        );
    if !actions.is_empty() {
        card = card.child(
            div()
                .mt(px(8.0))
                .flex()
                .flex_col()
                .gap(px(6.0))
                .children(actions),
        );
    }
    card
}

/// Why a connection-bound tab cannot show its results right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ResultConnectionState {
    Connecting,
    Disconnected,
    /// Redacted reason (initial connect failure or a lost session).
    Failed(String),
}

impl ResultConnectionState {
    /// `None` when the session is usable.
    fn from_status(status: ConnectionStatus) -> Option<Self> {
        match status {
            ConnectionStatus::Connected => None,
            ConnectionStatus::Connecting => Some(Self::Connecting),
            ConnectionStatus::Disconnected => Some(Self::Disconnected),
            ConnectionStatus::Failed(reason) => Some(Self::Failed(reason)),
        }
    }
}

fn save_connection_record(
    form: &ConnectionFormState,
    record: engine_storage::connection::SavedConnectionRecord,
    original_record: Option<&engine_storage::connection::SavedConnectionRecord>,
) -> Result<(), String> {
    engine_storage::connection::upsert_connection(&record)
        .map_err(|error| format!("Could not save connection: {error}"))?;
    {
        if form.save_password && !form.password.is_empty() {
            SystemStore::new()
                .save(SecretKind::Database, &record.id, &form.password)
                .map_err(|error| format!("Could not save connection password: {error}"))?;
        } else if !form.save_password
            || original_record
                .is_some_and(|original| original.save_password && !record.save_password)
        {
            SystemStore::new()
                .delete(SecretKind::Database, &record.id)
                .map_err(|error| format!("Could not clear connection password: {error}"))?;
        }
    }
    if let Some(ssh) = &form.ssh {
        let old = original_record.map(|original| {
            crate::ssh_tunnel::SshSettings::from_network_json(&original.network_settings_json)
        });
        // Unreadable stored settings: no telling whom the saved secrets were entered for.
        if original_record.is_some_and(|original| {
            crate::ssh_tunnel::SshSettings::parse(&original.network_settings_json).is_err()
        }) {
            crate::ssh_tunnel::clear_route_secrets(&SystemStore::new(), &record.id)
                .map_err(|error| format!("Could not clear SSH secrets: {error}"))?;
        }
        crate::ssh_tunnel::persist_secrets(
            &SystemStore::new(),
            &record.id,
            old.as_ref(),
            ssh,
            &form.ssh_secrets,
        )
        .map_err(|error| format!("Could not save SSH secrets: {error}"))?;
    }
    Ok(())
}

fn read_and_save_dialog_form(
    inputs: &ConnectionDialogInputs,
    initial_form: ConnectionFormState,
    original_record: Option<&engine_storage::connection::SavedConnectionRecord>,
    require_password: bool,
    cx: &gpui::App,
) -> Result<ConnectionFormState, String> {
    let mut form = inputs.read_form(cx, initial_form);
    if require_password {
        form.validate_recovery()?;
    }
    let record = form.to_record(current_time(), original_record)?;
    save_connection_record(&form, record, original_record)?;
    Ok(form)
}

impl AppRoot {
    pub(super) fn ensure_connected(
        &mut self,
        connection: String,
        request: Option<LoadRequest>,
        cx: &mut Context<Self>,
    ) {
        self.ensure_connected_with_uri(connection, None, request, cx);
    }

    pub(super) fn ensure_driver_connected(
        &mut self,
        saved: SavedConnection,
        uri_override: Option<RuntimeUri>,
        request: Option<LoadRequest>,
        cx: &mut Context<Self>,
    ) {
        let connection_key = saved.id.to_string();
        let Some(attempt) = self.sessions.begin_connect(&connection_key) else {
            if let Some(request) = request {
                if let Some(generation) = self.navigator.load_generation(&request) {
                    self.navigator_ui
                        .pending_loads
                        .entry(connection_key.clone())
                        .or_default()
                        .push((request, generation));
                }
            }
            return;
        };
        if let Some(request) = request {
            if let Some(generation) = self.navigator.load_generation(&request) {
                self.navigator_ui
                    .pending_loads
                    .entry(connection_key.clone())
                    .or_default()
                    .push((request, generation));
            }
        }

        if self.prompt_missing_driver(&saved, &uri_override, attempt, cx) {
            return;
        }
        self.run_driver_connect(saved, uri_override, attempt, cx);
    }

    #[allow(clippy::result_large_err)] // the closure's `Err` is engine-connect's `ConnectFailure`; boxing its variants touches every consumer
    /// The connect attempt `attempt` of `saved` on its driver: load the
    /// installed driver, open the SSH tunnel when the connection has one, and open the
    /// session. Also what a missing-driver prompt resumes once the driver is
    /// installed (or abandons to report the usual "not installed" failure), and what a
    /// trusted host key or a typed SSH secret resumes.
    pub(super) fn run_driver_connect(
        &mut self,
        saved: SavedConnection,
        uri_override: Option<RuntimeUri>,
        attempt: u64,
        cx: &mut Context<Self>,
    ) {
        let connection_key = saved.id.to_string();
        let driver_id = saved.kind.driver_id().to_string();
        let connect_timeout = self.effective_for(&connection_key).connect_timeout_secs;
        let (connector, prompts) = (self.connector(), self.tunnel_prompts());
        let (retry_saved, retry_uri) = (saved.clone(), uri_override.clone());
        // The tunnel (when the connection has one) is opened by the connector, on the background
        // thread below, and the driver connects to its local port. The session's URI stays the URI
        // as the user wrote it: later sessions rewrite it again with their own tunnel.
        let request = self.connect_request(saved, uri_override, connect_timeout);
        // Blocking work (the SSH handshake may wait for the user's answer) belongs on a thread of
        // its own, never on the executor that also runs the UI's callbacks.
        let connect_task = cx.background_executor().spawn(async move {
            smol::unblock(move || {
                connector
                    .open_request(request, &prompts)
                    .map(Connected::from)
            })
            .await
        });
        let connection_key_task = connection_key.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = connect_task.await;
            let _ = this.update(cx, |this, cx| {
                this.on_driver_connect_finished(&connection_key_task, cx);
                match result {
                    Ok(connected) => {
                        let Connected {
                            handle,
                            session,
                            connection_id,
                            dialect,
                            server_version,
                            uri: session_uri,
                            tunnel,
                        } = connected;
                        if !this
                            .sessions
                            .finish_connect(&connection_key_task, attempt, Ok(()))
                        {
                            this.discard_pending_runs(
                                &connection_key_task,
                                attempt,
                                "Connection changed while connecting; run again",
                                cx,
                            );
                            cx.background_executor()
                                .spawn(async move {
                                    smol::unblock(move || {
                                        if let Ok(mut session) = session.lock() {
                                            let _ = session.disconnect();
                                        }
                                        // The tunnel goes after its session.
                                        drop(tunnel);
                                    })
                                    .await;
                                })
                                .detach();
                            cx.notify();
                            return;
                        }
                        if let Some(live) = tunnel {
                            // Credentials stay in memory for the connection's lifetime.
                            if !live.access.secrets.is_empty() {
                                this.tunnel_ui.secrets.insert(
                                    connection_key_task.clone(),
                                    live.access.secrets.clone(),
                                );
                            }
                            this.tunnel_ui
                                .live
                                .insert(connection_key_task.clone(), live);
                        }
                        // The session's capabilities are the handle's: read
                        // them without taking the session lock.
                        this.driver_runtime.capabilities.insert(
                            connection_key_task.clone(),
                            Arc::new(connection_capabilities(handle.capabilities(), dialect)),
                        );
                        this.sync_navigator_kinds(&connection_key_task);
                        match server_version {
                            Some(version) => {
                                this.driver_runtime
                                    .server_versions
                                    .insert(connection_key_task.clone(), version);
                            }
                            None => {
                                this.driver_runtime.server_versions.remove(&connection_key_task);
                            }
                        }
                        this.driver_runtime
                            .session_handles
                            .insert(connection_key_task.clone(), handle.clone());
                        this.driver_runtime
                            .session_uris
                            .insert(connection_key_task.clone(), session_uri);
                        this.driver_ui.handles.insert(driver_id.clone(), handle);
                        this.driver_runtime
                            .sessions
                            .insert(connection_key_task.clone(), session);
                        this.driver_runtime
                            .connection_ids
                            .insert(connection_key_task.clone(), connection_id);
                        this.resume_pending_runs(&connection_key_task, cx);
                        this.ensure_auto_refresh_for_connection(&connection_key_task, cx);
                        this.navigator.enable_provider(&connection_key_task);
                        this.sync_sidebar_connections(cx);
                        if let Some(requests) =
                            this.navigator_ui.pending_loads.remove(&connection_key_task)
                        {
                            for (request, _) in requests {
                                if let Some(request) = this
                                    .navigator
                                    .toggle_expansion(&request.parent_id())
                                    .request
                                {
                                    this.dispatch_provider_load(request, cx);
                                }
                            }
                        }
                        this.navigator_ui.generation = this.navigator.generation();
                        this.rerun_tabs_after_reconnect(&connection_key_task, cx);
                    }
                    Err(failure) => {
                        this.handle_connect_failure(retry_saved, retry_uri, attempt, failure, cx);
                    }
                }
                cx.notify();
            });
        });
        task.detach();
    }

    pub(super) fn ensure_connected_with_uri(
        &mut self,
        connection: String,
        uri_override: Option<RuntimeUri>,
        request: Option<LoadRequest>,
        cx: &mut Context<Self>,
    ) {
        if self.scenario_ui.service.is_some() {
            if self.sessions.is_connected(&connection) {
                if let Some(request) = request {
                    self.dispatch_scenario_load(request, cx);
                }
                return;
            }
            if matches!(
                self.sessions.status(&connection),
                ConnectionStatus::Connecting
            ) {
                if let Some(request) = request {
                    if let Some(generation) = self.navigator.load_generation(&request) {
                        self.navigator_ui
                            .pending_loads
                            .entry(connection)
                            .or_default()
                            .push((request, generation));
                    }
                }
                return;
            }
            let name = self
                .saved_connections
                .iter()
                .find(|saved| saved.matches_id(&connection))
                .map(|saved| saved.name.clone())
                .unwrap_or_default();
            self.start_scenario_connect(connection, name, request, cx);
            return;
        }
        if uri_override.is_some()
            && matches!(
                self.sessions.status(&connection),
                ConnectionStatus::Connecting
            )
        {
            self.sessions.disconnect(&connection);
            self.sync_sidebar_connections(cx);
            self.navigator_ui.pending_loads.remove(&connection);
            self.navigator.reset_node(&NavigatorNodeId::Connection {
                connection: connection.clone(),
            });
            self.navigator_ui.generation = self.navigator.generation();
            cx.notify();
        }
        // A session that dropped (`*.not_connected`) keeps its dead driver
        // handle until the user reconnects; release it so this attempt
        // starts clean instead of reusing it.
        if matches!(
            self.sessions.status(&connection),
            ConnectionStatus::Failed(_)
        ) && self.driver_runtime.sessions.contains_key(&connection)
        {
            self.release_connection(&connection, true, cx);
        }
        let has_existing_session = self.sessions.is_connected(&connection)
            || self.driver_runtime.sessions.contains_key(&connection);
        if has_existing_session {
            if uri_override.is_none() {
                if let Some(request) = request {
                    self.dispatch_provider_load(request, cx);
                }
                return;
            }
            self.release_connection(&connection, true, cx);
        }

        let Some(saved) = self
            .saved_connections
            .iter()
            .find(|conn| conn.matches_id(&connection))
            .cloned()
        else {
            if let Some(request) = &request {
                let error = "Saved connection not found".to_string();
                self.report_error(Area::Navigator, Some(&connection), &error);
                if let Some(generation) = self.navigator.load_generation(request) {
                    self.navigator.apply_load(request, generation, Err(error));
                }
            }
            self.navigator_ui.generation = self.navigator.generation();
            cx.notify();
            return;
        };

        self.ensure_driver_connected(saved, uri_override, request, cx);
    }

    pub(super) fn finish_connection_test(&mut self, generation: u64) -> bool {
        if self.connection_ui.test_generation != generation {
            return false;
        }
        self.connection_ui.test_in_flight.set(false);
        true
    }

    #[allow(clippy::result_large_err)] // the closure's `Err` is engine-connect's `ConnectFailure`; boxing its variants touches every consumer
    /// Runs "Test" for the dialog form and reports the result inline through
    /// `status` (no modal): `Connected in N ms` or the server's error.
    pub(super) fn start_connection_test(
        &mut self,
        form: ConnectionFormState,
        original_record: Option<engine_storage::connection::SavedConnectionRecord>,
        window: gpui::AnyWindowHandle,
        status: std::rc::Rc<std::cell::RefCell<crate::connection_dialog::TestStatus>>,
        cx: &mut Context<Self>,
    ) {
        use crate::connection_dialog::TestStatus;
        if self.connection_ui.test_in_flight.get() {
            return;
        }
        let refresh = move |cx: &mut gpui::App| {
            let _ = window.update(cx, |_, window, _| window.refresh());
        };
        let runtime_uri = match runtime_uri_for_form(&form, original_record.as_ref()) {
            Ok(uri) => uri,
            Err(error) => {
                *status.borrow_mut() = TestStatus::Failure(error);
                cx.notify();
                refresh(cx);
                return;
            }
        };
        self.connection_ui.test_generation = self.connection_ui.test_generation.wrapping_add(1);
        self.connection_ui.test_in_flight.set(true);
        let generation = self.connection_ui.test_generation;
        *status.borrow_mut() = TestStatus::Running;
        let started = Instant::now();
        if let Some(service) = self.scenario_ui.service.as_mut() {
            let _ = runtime_uri;
            let (response, delay) = service.test(&form.name);
            let outcome = response.outcome;
            self.emit_scenario_event("test-start", false, "test", None);
            let executor = cx.background_executor().clone();
            let task = cx.background_executor().spawn(async move {
                executor.timer(delay).await;
                outcome
            });
            let task = cx.spawn(async move |this, cx| {
                let outcome = task.await;
                let should_show = this.update(cx, |this, cx| {
                    let should_show = this.finish_connection_test(generation);
                    if should_show {
                        this.record_scenario_test_terminal();
                        let event = match outcome {
                            FixtureOutcome::Success => "test-success",
                            FixtureOutcome::Error | FixtureOutcome::Timeout => "test-error",
                        };
                        let outcome_name = match outcome {
                            FixtureOutcome::Success => "success",
                            FixtureOutcome::Error => "error",
                            FixtureOutcome::Timeout => "timeout",
                        };
                        this.emit_scenario_event(event, false, "test", Some(outcome_name));
                        cx.notify();
                    }
                    should_show
                });
                if should_show.unwrap_or(false) {
                    *status.borrow_mut() = match outcome {
                        FixtureOutcome::Success => TestStatus::Success {
                            millis: started.elapsed().as_millis() as u64,
                        },
                        FixtureOutcome::Error => {
                            TestStatus::Failure("simulated fixture error".into())
                        }
                        FixtureOutcome::Timeout => {
                            TestStatus::Failure("simulated fixture timeout".into())
                        }
                    };
                    cx.update(|cx| refresh(cx));
                }
            });
            task.detach();
            return;
        }
        cx.notify();
        refresh(cx);
        let (connector, prompts) = (self.connector(), self.tunnel_prompts());
        // Through the SSH tunnel of the form, when its tab is on (then one is required); the tunnel
        // lives until the test session is closed.
        let request = self.form_request(&form, original_record.as_ref(), runtime_uri);
        let (retry_form, retry_original, retry_status) =
            (form.clone(), original_record.clone(), status.clone());
        let task = cx.background_executor().spawn(async move {
            smol::unblock(move || {
                connector
                    .open_request(request, &prompts)?
                    .close()
                    .map_err(|error| crate::ssh_tunnel::ConnectFailure::Message(error.to_string()))
            })
            .await
        });
        let task = cx.spawn(async move |this, cx| {
            let result = task.await;
            let should_show = this.update(cx, |this, cx| {
                let should_show = this.finish_connection_test(generation);
                if let (true, Err(failure)) = (should_show, &result) {
                    this.report_warning(
                        Area::Connect,
                        None,
                        format!("Connection test failed: {}", failure.message()),
                    );
                }
                if should_show {
                    cx.notify();
                }
                should_show
            });
            if should_show.unwrap_or(false) {
                match result {
                    Ok(()) => {
                        *status.borrow_mut() = TestStatus::Success {
                            millis: started.elapsed().as_millis() as u64,
                        };
                    }
                    Err(crate::ssh_tunnel::ConnectFailure::HostKeyUnknown(request)) => {
                        let _ = this.update(cx, |this, cx| {
                            let again: super::tunnel_ui::RootCallback = {
                                let (form, original, status) = (
                                    retry_form.clone(),
                                    retry_original.clone(),
                                    retry_status.clone(),
                                );
                                std::rc::Rc::new(move |this, cx| {
                                    this.start_connection_test(
                                        form.clone(),
                                        original.clone(),
                                        window,
                                        status.clone(),
                                        cx,
                                    )
                                })
                            };
                            this.ask_to_trust_for_test(request, window, status.clone(), again, cx);
                        });
                    }
                    Err(failure) => {
                        *status.borrow_mut() = TestStatus::Failure(
                            crate::sessions::redact_secrets(&failure.message()),
                        );
                        if let crate::ssh_tunnel::ConnectFailure::HostKeyRejected(problem, _) =
                            failure
                        {
                            let _ =
                                this.update(cx, |this, cx| this.show_host_key_problem(problem, cx));
                        }
                    }
                }
                cx.update(|cx| refresh(cx));
            }
        });
        task.detach();
    }

    pub(super) fn connection_tab_state(&self, connection_id: &str) -> ConnectionTabState {
        let Some(saved) = self
            .saved_connections
            .iter()
            .find(|connection| connection.matches_id(connection_id))
        else {
            return ConnectionTabState::Missing;
        };
        connection_dashboard::state_for(
            true,
            !saved.disabled,
            self.sessions.status(&saved.id.to_string()),
        )
    }

    pub(super) fn retry_connection(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let Some(connection_id) = self
            .workspace
            .tabs()
            .iter()
            .find(|tab| tab.id == tab_id)
            .map(|tab| tab.connection_id.clone())
        else {
            return;
        };
        if !connection_id.is_empty() && !self.sessions.is_connected(&connection_id) {
            self.ensure_connected(connection_id, None, cx);
        }
    }

    /// Which connection state, if any, the results of `tab` must surface.
    ///
    /// Rule: only a connection *known* to be unusable counts. `Failed`
    /// (connect error or a dropped session), `Connecting`, and `Disconnected`
    /// after the connection was live (`ConnectionSessions::was_dropped`)
    /// always count. A `Disconnected` connection that never connected counts
    /// only outside UI-scenario mode: fixtures open tabs with result data on
    /// connections nobody connected, and must keep showing it.
    ///
    /// Presentation is chosen by [`Self::tab_holds_results`]: a tab with a
    /// result keeps it visible under a slim banner; one without gets the full
    /// placeholder. Tabs without a saved connection (or unbound) never match.
    /// Cheap: a few hash lookups per render.
    pub(super) fn result_connection_state(&self, tab: &Tab) -> Option<ResultConnectionState> {
        if tab.kind != TabKind::Query || tab.connection_id.is_empty() {
            return None;
        }
        if !self
            .saved_connections
            .iter()
            .any(|saved| saved.matches_id(&tab.connection_id))
        {
            return None;
        }
        let status = self.sessions.status(&tab.connection_id);
        if matches!(status, ConnectionStatus::Disconnected)
            && self.scenario_ui.service.is_some()
            && !self.sessions.was_dropped(&tab.connection_id)
        {
            return None;
        }
        ResultConnectionState::from_status(status)
    }

    /// Whether the tab already holds a result grid worth keeping on screen.
    pub(super) fn tab_holds_results(&self, tab_id: u64) -> bool {
        self.workspace
            .runtime(tab_id)
            .is_some_and(|runtime| runtime.grid.column_count() > 0 || runtime.plan.is_some())
    }

    fn reconnect_button(&self, tab_id: u64, cx: &mut Context<Self>) -> Button {
        ui::primary_action_button(
            "connection-lost-reconnect",
            tab_id,
            "Reconnect to the database",
        )
        .debug_selector(|| "connection-lost-reconnect".into())
        .icon(Icon::new(SidebarGlyph::Connected))
        .on_click(cx.listener(move |this, _, _, cx| {
            this.retry_connection(tab_id, cx);
        }))
    }

    fn edit_connection_button(
        &self,
        tab_id: u64,
        saved: Option<SavedConnection>,
        require_password: bool,
        cx: &mut Context<Self>,
    ) -> Button {
        Button::new(("connection-lost-edit", tab_id))
            .debug_selector(|| "connection-lost-edit".into())
            .label("Edit Connection")
            .compact()
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_connection_dialog_with_requirement(
                    saved.clone(),
                    require_password,
                    window,
                    cx,
                );
            }))
    }

    /// Name, reason and whether the reason asks for a password re-entry.
    fn result_connection_details(
        &self,
        connection_id: &str,
        state: &ResultConnectionState,
    ) -> (Option<SavedConnection>, String, bool) {
        let saved = self
            .saved_connections
            .iter()
            .find(|saved| saved.matches_id(connection_id))
            .cloned();
        match state {
            ResultConnectionState::Failed(reason) => (
                saved,
                reason.clone(),
                crate::connections::is_missing_password_error(reason),
            ),
            _ => {
                let name = saved
                    .as_ref()
                    .map(|saved| saved.name.clone())
                    .unwrap_or_default();
                (saved, name, false)
            }
        }
    }

    /// Centered "Disconnected" card replacing the results of a tab that has
    /// none to show, reusing the connection tab's state card and
    /// `retry_connection`.
    pub(super) fn render_result_connection_state(
        &self,
        tab_id: u64,
        connection_id: &str,
        state: ResultConnectionState,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (saved, description, require_password) =
            self.result_connection_details(connection_id, &state);
        let icon = Some(Icon::new(SidebarGlyph::Connected));
        let card = match state {
            ResultConnectionState::Connecting => {
                connection_state_card_with(icon, "Connecting…", description, Vec::new())
            }
            ResultConnectionState::Disconnected | ResultConnectionState::Failed(_) => {
                let failed = matches!(state, ResultConnectionState::Failed(_));
                let mut actions = vec![self.reconnect_button(tab_id, cx).into_any_element()];
                if failed {
                    actions.push(
                        self.edit_connection_button(tab_id, saved, require_password, cx)
                            .into_any_element(),
                    );
                }
                connection_state_card_with(icon, "Disconnected", description, actions)
            }
        };
        div()
            .debug_selector(|| "connection-lost-placeholder".into())
            .flex()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .items_center()
            .justify_center()
            .p(px(16.0))
            .child(
                // The card's auto side margins (block-layout centering for the
                // dashboard tab) would swallow the free space of this flex
                // row and push it to the edge; reset them and let the row
                // center it instead.
                card.ml(px(0.0)).mr(px(0.0)).items_center().text_center(),
            )
            .into_any_element()
    }

    /// Slim, non-blocking strip above a stale result: the grid stays usable
    /// while the connection is down.
    pub(super) fn render_result_connection_banner(
        &self,
        tab_id: u64,
        connection_id: &str,
        state: ResultConnectionState,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (saved, detail, require_password) =
            self.result_connection_details(connection_id, &state);
        let connecting = matches!(state, ResultConnectionState::Connecting);
        let failed = matches!(state, ResultConnectionState::Failed(_));
        let label = match (&state, detail.is_empty()) {
            (ResultConnectionState::Connecting, _) => "Connecting…".to_string(),
            (_, true) => "Disconnected".to_string(),
            _ => format!("Disconnected — {detail}"),
        };
        div()
            .debug_selector(|| "connection-lost-banner".into())
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(8.0))
            .h(px(32.0))
            .px(px(12.0))
            .bg(theme::surface())
            .border_b_1()
            .border_color(theme::border())
            .text_size(px(12.0))
            .text_color(theme::text_muted())
            .child(Icon::new(SidebarGlyph::Connected).size(px(14.0)))
            .child(div().flex_1().min_w_0().truncate().child(label))
            .when(!connecting, |this| {
                this.child(self.reconnect_button(tab_id, cx))
            })
            .when(failed, |this| {
                this.child(self.edit_connection_button(tab_id, saved, require_password, cx))
            })
            .into_any_element()
    }

    /// A driver said `*.not_connected` for `tab_id`'s connection: fail the
    /// session so every tab of the connection shows the reconnect state. The
    /// dead driver session handle is released by the next connect attempt.
    ///
    /// A request that fails on a connection whose SSH tunnel is gone counts as lost too, with
    /// the tunnel's code: this is the coarse check on the existing error path (nothing polls).
    pub(super) fn note_connection_lost(&mut self, tab_id: u64, error: &str) {
        let tunnel_down = self.tab_tunnel_is_down(tab_id);
        self.note_lost(tab_id, crate::sessions::is_connection_lost_error(error), tunnel_down);
    }

    /// [`Self::note_connection_lost`] for a typed driver error (no text parse).
    pub(super) fn note_driver_connection_lost(&mut self, tab_id: u64, error: &DriverQueryError) {
        let tunnel_down = self.tab_tunnel_is_down(tab_id);
        self.note_lost(tab_id, error.is_connection_lost(), tunnel_down);
    }

    pub(super) fn tab_tunnel_is_down(&self, tab_id: u64) -> bool {
        self.workspace
            .tab(tab_id)
            .is_some_and(|tab| self.tunnel_failure(&tab.connection_id).is_some())
    }

    /// The connection's shared session of `tab_id` is lost.
    pub(super) fn mark_tab_connection_lost(&mut self, tab_id: u64) {
        let Some(connection) = self
            .workspace
            .tab(tab_id)
            .map(|tab| tab.connection_id.clone())
            .filter(|connection| !connection.is_empty())
        else {
            return;
        };
        if self.mark_connection_lost(&connection) {
            self.sync_connected_ids();
        }
    }

    /// The tabs on screen: the active tab of every pane.
    pub(super) fn visible_tab_ids(&self) -> Vec<u64> {
        self.workspace
            .panes()
            .panes()
            .filter_map(|pane| pane.active())
            .collect()
    }

    /// Re-runs `tab_id` now when it is on screen, else when it is next
    /// activated. A table-data or Mongo find tab holds a result the driver
    /// can no longer vouch for (a write or a reconnect dropped its page).
    pub(super) fn refresh_tab_when_seen(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if self.visible_tab_ids().contains(&tab_id) {
            self.rerun_stale_tab(tab_id, cx);
        } else {
            self.connection_ui.rerun_on_activate.insert(tab_id);
        }
    }

    /// Re-runs a stale tab. Staged Mongo edits keep their rows only while the
    /// result stays, so with edits pending nothing runs and the document view
    /// says the result is out of date. `true` when a run is under way.
    pub(super) fn rerun_stale_tab(&mut self, tab_id: u64, cx: &mut Context<Self>) -> bool {
        if self
            .workspace
            .runtime(tab_id)
            .is_some_and(|runtime| runtime.mongo.has_pending_mutations())
        {
            if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
                if runtime.mongo.inspector.selected.is_some() {
                    runtime.mongo.inspector.loading = false;
                    runtime.mongo.inspector.error =
                        Some(super::results::mongo::controls::STALE_MONGO_PAGE.into());
                }
            }
            self.notify_tab(tab_id, cx);
            return false;
        }
        self.run_tab_query(tab_id, cx);
        self.workspace.runtime(tab_id).is_some_and(|runtime| {
            matches!(
                runtime.query.status(),
                crate::query::QueryStatus::Running { .. }
            )
        })
    }

    /// The driver dropped `page_ids` of `connection` (a mutation's
    /// `invalidated_page_ids`): every Mongo find tab of the connection that
    /// shows one of those pages is stale. Tabs on screen re-run now, the
    /// others when activated. `issuer` is the tab whose own commit re-runs it.
    pub(super) fn invalidate_driver_pages(
        &mut self,
        connection: &str,
        page_ids: &[u64],
        issuer: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        if page_ids.is_empty() {
            return;
        }
        let stale: Vec<u64> = self
            .workspace
            .tabs()
            .iter()
            .filter(|tab| {
                tab.connection_id == connection
                    && Some(tab.id) != issuer
                    && (tab.query_mode.is_mongo_find() || tab.query_mode.is_redis_key())
            })
            .filter(|tab| {
                self.workspace
                    .runtime(tab.id)
                    .and_then(|runtime| runtime.driver_page_id)
                    .is_some_and(|page_id| page_ids.contains(&page_id))
            })
            .map(|tab| tab.id)
            .collect();
        for tab_id in stale {
            self.refresh_tab_when_seen(tab_id, cx);
        }
    }

    /// After a successful (re)connect: table-data and Mongo find tabs that
    /// already ran re-run their current query (same page, filter, sort); the
    /// driver dropped their pages with the old session. The tabs on screen
    /// (the active one of every pane) run now, the rest on activation. SQL
    /// editor, script and aggregate tabs may hold writes, so they never auto-run.
    pub(super) fn rerun_tabs_after_reconnect(&mut self, connection: &str, cx: &mut Context<Self>) {
        let stale: Vec<u64> = self
            .workspace
            .tabs()
            .iter()
            .filter(|tab| {
                tab.kind == TabKind::Query
                    && tab.connection_id == connection
                    && (tab.query_mode.is_table_data()
                        || tab.query_mode.is_mongo_find()
                        || tab.query_mode.is_redis_key())
            })
            .filter(|tab| {
                self.workspace
                    .runtime(tab.id)
                    .is_some_and(|runtime| runtime.has_run() && runtime.query.can_run())
            })
            .map(|tab| tab.id)
            .collect();
        for tab_id in stale {
            self.refresh_tab_when_seen(tab_id, cx);
        }
    }

    /// Runs the deferred re-run for the tab that was just activated.
    pub(super) fn run_deferred_reconnect_rerun(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if self.connection_ui.rerun_on_activate.remove(&tab_id)
            && self.workspace.active_tab().map(|tab| tab.id) == Some(tab_id)
        {
            self.rerun_stale_tab(tab_id, cx);
        }
    }

    /// Closes the connection's session and every SSH tunnel of it, and forgets the SSH
    /// secrets that were only typed (a deliberate disconnect starts from scratch).
    pub(super) fn disconnect_connection(&mut self, connection_id: &str, cx: &mut Context<Self>) {
        // The sessions the connection's tabs opened for manual commit mode go with it (the user's
        // own Disconnect asked about open transactions before getting here).
        self.connection_transactions_closed(connection_id);
        self.release_connection(connection_id, false, cx);
    }

    /// Releases the dead session of `connection_id` before a run opens a new one. The tabs' own
    /// sessions are separate sessions: they stay.
    pub(super) fn release_dead_session(&mut self, connection_id: &str, cx: &mut Context<Self>) {
        self.release_connection(connection_id, false, cx);
    }

    /// [`Self::disconnect_connection`] for a reconnect: the typed secrets stay, the tunnel does
    /// not (a reconnect always opens a fresh one).
    fn release_connection(
        &mut self,
        connection_id: &str,
        keep_secrets: bool,
        cx: &mut Context<Self>,
    ) {
        if self.scenario_ui.service.is_some() {
            // No driver session to close: the fixture's session and tree go.
            self.tunnel_ui.release(connection_id, keep_secrets);
            self.sessions.disconnect(connection_id);
            self.navigator_ui.pending_loads.remove(connection_id);
            // A deleted connection's tree goes with its row, not before it.
            if self.saved_connections.iter().any(|saved| saved.matches_id(connection_id)) {
                self.navigator.reset_node(&NavigatorNodeId::Connection {
                    connection: connection_id.to_string(),
                });
                self.navigator_ui.generation = self.navigator.generation();
            }
            self.sync_sidebar_connections(cx);
            cx.notify();
            return;
        }
        self.cancel_visible_filter_names(connection_id, cx);
        self.saved_sql_ui.cancel(connection_id);
        self.cancel_auto_refresh_for_connection(connection_id);
        // A cleanup of an auto-commit run on the session that is going away holds nobody any more.
        self.transactions_released(connection_id);
        for tab_id in self
            .workspace
            .tabs()
            .iter()
            .filter(|tab| tab.connection_id == connection_id)
            .map(|tab| tab.id)
            .collect::<Vec<_>>()
        {
            // A tab on a session of its own (manual commit) runs on a healthy session of its
            // own: the shared one going away is no reason to cancel it or let go of its page.
            if self.driver_runtime.tab_sessions.contains_key(&tab_id) {
                continue;
            }
            self.cancel_tab_query(tab_id, cx);
            self.release_driver_page(tab_id);
        }
        let had_session = self.driver_runtime.sessions.contains_key(connection_id)
            || self
                .driver_runtime
                .connection_ids
                .contains_key(connection_id)
            || !matches!(
                self.sessions.status(connection_id),
                ConnectionStatus::Disconnected
            );
        // The tunnel is dropped after the session said goodbye through it.
        let tunnel = self.tunnel_ui.release(connection_id, keep_secrets);
        let session = self.driver_runtime.sessions.remove(connection_id);
        if session.is_some() || tunnel.is_some() {
            // Closing a tunnel can wait on the network: off the UI thread, after the session.
            let task = cx.background_executor().spawn(async move {
                smol::unblock(move || {
                    if let Some(session) = session {
                        if let Ok(mut session) = session.lock() {
                            let _ = session.disconnect();
                        }
                    }
                    drop(tunnel);
                })
                .await;
            });
            task.detach();
        }
        self.sessions.disconnect(connection_id);
        self.driver_runtime.connection_ids.remove(connection_id);
        self.driver_runtime.capabilities.remove(connection_id);
        self.driver_runtime.server_versions.remove(connection_id);
        self.driver_runtime.session_handles.remove(connection_id);
        self.driver_runtime.session_uris.remove(connection_id);
        self.sync_sidebar_connections(cx);
        if !had_session && !self.navigator_ui.pending_loads.contains_key(connection_id) {
            return;
        }
        self.navigator_ui.pending_loads.remove(connection_id);
        self.navigator.reset_node(&NavigatorNodeId::Connection {
            connection: connection_id.to_string(),
        });
        self.navigator_ui.generation = self.navigator.generation();
        cx.notify();
    }

    pub(super) fn refresh_saved_connections(&mut self, cx: &mut Context<Self>) {
        if self.scenario_ui.service.is_some() {
            return;
        }
        let records = match engine_storage::connection::list_connections() {
            Ok(records) => records,
            Err(error) => {
                self.sidebar.notice = Some(format!("Could not refresh connections: {error}"));
                self.report_error(
                    Area::Connect,
                    None,
                    format!("Could not refresh connections: {error}"),
                );
                cx.notify();
                return;
            }
        };
        let saved = records.iter().map(SavedConnection::from_record).collect();
        self.connection_policy = connection_policy::ConnectionPolicy::from_records(&records);
        self.apply_saved_connections(saved, cx);
        self.assistant_sync(cx);
    }

    /// Installs a new saved-connection list after add / edit / delete:
    /// disconnects removed connections and updates the navigator in place
    /// (untouched connections keep their expansion and loaded children).
    pub(super) fn apply_saved_connections(
        &mut self,
        saved: Vec<SavedConnection>,
        cx: &mut Context<Self>,
    ) {
        self.saved_connections = saved;
        let live: HashSet<_> = self
            .saved_connections
            .iter()
            .map(|connection| connection.id.to_string())
            .collect();
        for removed in self
            .sessions
            .status_keys()
            .into_iter()
            .filter(|id| !live.contains(id))
            .collect::<Vec<_>>()
        {
            self.disconnect_connection(&removed, cx);
        }
        self.sidebar.reload_colors();
        self.sync_connection_output_limits();
        self.release_idle_own_sessions(cx);
        self.navigator
            .set_connections(self.saved_connections.clone());
        self.reconcile_sidebar_placements(cx);
        cx.notify();
    }

    pub(super) fn refresh_connection_id(&mut self, connection_id: &str, cx: &mut Context<Self>) {
        let row = self.navigator.visible_rows().into_iter().find(|row| {
            row.id
                == NavigatorNodeId::Connection {
                    connection: connection_id.to_string(),
                }
        });
        if let Some(row) = row {
            self.refresh_connection(&row, cx);
        }
    }

    pub(super) fn toggle_connection_pin(&mut self, connection_id: &str, cx: &mut Context<Self>) {
        if self.scenario_ui.service.is_some() {
            return;
        }
        let Some(connection) = self
            .saved_connections
            .iter_mut()
            .find(|connection| connection.matches_id(connection_id))
        else {
            return;
        };
        let next_pin = !connection.is_pinned;
        let storage_id = crate::connections::storage_connection_id(&connection.id.to_string());
        match engine_storage::connection::set_connection_pinned(&storage_id, next_pin) {
            Ok(()) => self.refresh_saved_connections(cx),
            Err(error) => {
                self.sidebar.notice = Some(format!("Could not pin connection: {error}"));
                self.report_error(
                    Area::Connect,
                    Some(connection_id),
                    format!("Could not pin connection: {error}"),
                );
                cx.notify();
            }
        }
    }

    pub(super) fn copy_connection_string(&mut self, connection_id: &str, cx: &mut Context<Self>) {
        self.copy_connection_info(
            connection_id,
            super::connection_menu::CopyInfo::ConnectionUrl,
            cx,
        );
    }

    pub(super) fn delete_connection_with_confirmation(
        &mut self,
        connection_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.scenario_ui.service.is_some() {
            return;
        }
        let Some(connection) = self
            .saved_connections
            .iter()
            .find(|connection| connection.matches_id(&connection_id))
            .cloned()
        else {
            return;
        };
        // An open transaction of a tab on it is asked about first; the delete is asked for again
        // once it ended.
        let tabs = self.tabs_of_connections(std::slice::from_ref(&connection_id));
        let again = connection_id.clone();
        if self.guard_transactions(tabs, "Deleting the connection ends their sessions.", window, cx, move |this, window, cx| {
            this.delete_connection_with_confirmation(again, window, cx)
        }) {
            return;
        }
        let root = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let root = root.clone();
            let connection_id = connection_id.clone();
            dialog
                .title("Delete Connection")
                .child(format!("Delete {}?", connection.name))
                .button_props(
                    gpui_component::dialog::DialogButtonProps::default().ok_text("Delete"),
                )
                .confirm()
                .on_ok(move |_, _, cx| {
                    root.update(cx, |this, cx| {
                        this.delete_connection(&connection_id, cx).is_ok()
                    })
                    .unwrap_or(false)
                })
        });
        cx.notify();
    }

    pub(super) fn delete_connection(
        &mut self,
        connection_id: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if self.scenario_ui.service.is_some() {
            return Err("Cannot delete connections in a UI scenario".into());
        }
        // A transaction opened since the question was asked is not lost without one.
        if self.open_transactions_on(&[connection_id.to_owned()]) {
            return Err("A tab of this connection has an open transaction: commit or roll it back first.".into());
        }
        let storage_id = crate::connections::storage_connection_id(connection_id);
        if let Err(error) = engine_storage::connection::delete_connection(&storage_id) {
            let message = format!("Could not delete connection: {error}");
            self.sidebar.notice = Some(message.clone());
            self.report_error(Area::Connect, Some(connection_id), &message);
            cx.notify();
            return Err(message);
        }
        self.disconnect_connection(connection_id, cx);
        self.revoke_agent_access(connection_id);
        // Its saved assistant conversations go with it.
        self.assistant_forget_connection(connection_id, cx);
        // Clearing the secrets is up to 20 keychain calls (the database, proxy and SSH secrets of
        // the connection and of each jump host): never on the UI thread.
        let cleanup = cx.background_executor().spawn({
            let storage_id = storage_id.clone();
            async move { smol::unblock(move || SystemStore::new().delete_all(&storage_id)).await }
        });
        cx.spawn(async move |this, cx| {
            let Err(error) = cleanup.await else { return };
            let warning = format!(
                "Connection deleted, but could not clear its {} secrets: {error}",
                crate::platform::CREDENTIAL_STORE_LABEL
            );
            let _ = this.update(cx, |this, cx| {
                this.report_warning(Area::Connect, None, &warning);
                this.sidebar.notice = Some(match this.sidebar.notice.take() {
                    Some(other) => format!("{warning}; {other}"),
                    None => warning,
                });
                cx.notify();
            });
        })
        .detach();

        let tabs: Vec<u64> = self
            .workspace
            .tabs()
            .iter()
            .filter(|tab| tab.kind == TabKind::Connection && tab.connection_id == connection_id)
            .map(|tab| tab.id)
            .collect();
        for tab_id in tabs {
            self.close_tab(tab_id, cx);
        }
        self.refresh_saved_connections(cx);
        if self.sidebar.error.is_none() {
            if let Err(error) = self.change_sidebar_preferences(
                |prefs| {
                    prefs.remove_connection(connection_id);
                    Ok(())
                },
                cx,
            ) {
                self.sidebar.notice = Some(format!(
                    "Connection deleted, but sidebar placement could not be updated: {error}"
                ));
                self.report_warning(
                    Area::Connect,
                    None,
                    format!(
                        "Connection deleted, but sidebar placement could not be updated: {error}"
                    ),
                );
            }
        } else {
            self.sidebar.notice = Some("Connection deleted, but sidebar preferences need repair before placement can be updated".into());
            self.report_warning(Area::Connect, None, "Connection deleted, but sidebar preferences need repair before placement can be updated");
        }
        cx.notify();
        Ok(())
    }

    pub(super) fn open_connection_dialog(
        &mut self,
        existing: Option<SavedConnection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_connection_dialog_with_requirement(existing, false, window, cx);
    }

    pub(super) fn open_connection_dialog_with_requirement(
        &mut self,
        existing: Option<SavedConnection>,
        require_password: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.invalidate_connection_test();
        let mut form = existing
            .as_ref()
            .map(ConnectionFormState::from_saved)
            .unwrap_or_else(|| ConnectionFormState::new(ConnectionKind::Postgres));
        let original_record = if self.scenario_ui.service.is_some() {
            None
        } else {
            existing.as_ref().and_then(|saved| {
                let id = saved.id.to_string();
                engine_storage::connection::get_connection(&id.to_uppercase())
                    .ok()
                    .flatten()
                    .or_else(|| {
                        engine_storage::connection::get_connection(&id)
                            .ok()
                            .flatten()
                    })
            })
        };
        if let Some(record) = &original_record {
            form.color_hex = Some(record.color_hex.clone());
        }
        form.ssh = Some(
            original_record
                .as_ref()
                .map(|record| {
                    crate::ssh_tunnel::SshSettings::from_network_json(&record.network_settings_json)
                })
                .or_else(|| {
                    // Fixture connections have no record: the app may hold their settings.
                    existing.as_ref().and_then(|saved| {
                        self.tunnel_ui
                            .settings_override
                            .get(&saved.id.to_string())
                            .cloned()
                    })
                })
                .unwrap_or_default(),
        );
        // The type and overrides come from the record, or (fixture
        // connections have none) from what the app loaded.
        form.profile = Some(match &original_record {
            Some(record) => crate::settings::connection::ConnectionProfile::from_record(record),
            None => existing
                .as_ref()
                .and_then(|saved| {
                    self.connection_policy
                        .profile(&saved.id.to_string())
                        .cloned()
                })
                .unwrap_or_default(),
        });
        let inputs = ConnectionDialogInputs::new(window, cx, &form, &self.settings);
        if original_record.as_ref().is_some_and(|record| {
            crate::ssh_tunnel::SshSettings::parse(&record.network_settings_json).is_err()
        }) {
            inputs.ssh.note_unreadable_settings();
        }
        if self.scenario_ui.service.as_ref().is_some_and(|service| {
            service.scenario().kind == crate::ui_scenario::ScenarioKind::ConnectionOverrides
        }) {
            inputs
                .tab
                .set(crate::connection_dialog::ConfigTab::Overrides);
        }
        inputs.set_driver_availability(self.driver_availability());
        #[cfg(test)]
        {
            self.connection_ui.last_inputs = Some(inputs.clone());
        }
        let title = if existing.is_some() {
            "Edit Connection"
        } else {
            "New Connection"
        };
        let ok_text = if require_password {
            "Save & Connect"
        } else {
            "Save"
        };
        let scenario_enabled = self.scenario_ui.service.is_some();
        #[cfg(feature = "ui-scenarios")]
        let inputs = {
            let mut inputs = inputs;
            inputs.set_scenario_enabled(scenario_enabled);
            inputs
        };
        let root = cx.entity().downgrade();
        {
            // The tab's "Test tunnel" button.
            let (tab_inputs, initial_form, record, root) = (
                inputs.clone(),
                form.clone(),
                original_record.clone(),
                root.clone(),
            );
            inputs.ssh.set_test_handler(move |window, cx| {
                let form = tab_inputs.read_form(cx, initial_form.clone());
                let status = tab_inputs.ssh.test_status.clone();
                if let Err(error) = form.validate() {
                    *status.borrow_mut() = crate::connection_dialog::TestStatus::Failure(error);
                    return;
                }
                let window_handle = window.window_handle();
                let _ = root.update(cx, |this, cx| {
                    this.start_tunnel_test(form, record.clone(), window_handle, status, cx);
                });
            });
        }
        #[cfg(feature = "ui-scenarios")]
        let last_scenario_state = scenario_enabled.then(|| std::cell::RefCell::new(None));
        #[cfg(feature = "ui-scenarios")]
        if scenario_enabled {
            let seed = std::env::var("TABLEX_SEED_TUNNEL").unwrap_or_default();
            if !seed.is_empty() && existing.is_none() && !seed.split(',').all(|t| t == "connected")
            {
                inputs.seed_tunnel_capture(
                    seed.split(',').any(|token| token == "test"),
                    seed.split(',').any(|token| token == "chain"),
                    window,
                    cx,
                );
            }
        }
        #[cfg(feature = "ui-scenarios")]
        if scenario_enabled && existing.is_none() {
            if let Ok(seed) = std::env::var("TABLEX_SEED_TLS") {
                inputs.seed_tls_capture(&seed, window, cx);
            }
        }
        window.open_dialog(cx, move |dialog, _window, _cx| {
            #[cfg(feature = "ui-scenarios")]
            let dialog_id = inputs.scenario_id();
            #[cfg(feature = "ui-scenarios")]
            if let Some(previous) = last_scenario_state.as_ref() {
                let state = inputs.scenario_state(_cx);
                if previous.borrow().as_ref() != Some(&state) {
                    let first_frame = previous.borrow().is_none();
                    *previous.borrow_mut() = Some(state.clone());
                    crate::ui_scenario::emit_ui_state_for_dialog(
                        state,
                        inputs.scenario_id(),
                        _window,
                    );
                    if first_frame {
                        crate::ui_scenario::emit_ready();
                    }
                }
            }
            let viewport = _window.viewport_size();
            let content = inputs.render_with_height(connection_dialog_height(viewport.height));
            let width = connection_dialog_width(viewport.width, inputs.is_picker());
            dialog
                .w(width)
                .when(inputs.is_picker(), |this| {
                    this.px(px(18.0)).pt(px(16.0)).pb(px(0.0))
                })
                .when(!inputs.is_picker(), |this| this.px(px(20.0)))
                .title(
                    div()
                        .text_size(px(15.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .child(title),
                )
                .child(content)
                .footer({
                    let inputs = inputs.clone();
                    let initial_form = form.clone();
                    let original_record = original_record.clone();
                    let root = root.clone();
                    #[cfg(feature = "ui-scenarios")]
                    let scenario_enabled = scenario_enabled;
                    let ok = |_: &mut Window, _: &mut gpui::App| {
                        gpui_component::dialog::DialogAction::new()
                            .child(
                                Button::new("ok")
                                    .debug_selector(|| "connection-save".into())
                                    .label(ok_text)
                                    .compact()
                                    .primary(),
                            )
                            .into_any_element()
                    };
                    let buttons = (|window: &mut Window, cx: &mut gpui::App| {
                        if inputs.is_picker() {
                            let inputs_for_next = inputs.clone();
                            let next = Button::new("connection-picker-next")
                                .label("Next")
                                .debug_selector(|| "connection-picker-next".into())
                                .compact()
                                .outline()
                                .child(
                                    gpui_component::Icon::empty()
                                        .path("icons/chevron-right.svg")
                                        .size(px(14.0)),
                                )
                                .disabled(!inputs.picker_selection_is_visible())
                                .on_click(move |_, _, cx| {
                                    inputs_for_next.set_step(
                                        crate::connection_dialog::ConnectionDialogStep::Config,
                                        cx,
                                    );
                                });
                            #[cfg(feature = "ui-scenarios")]
                            let next = {
                                let enabled = inputs.picker_selection_is_visible();
                                let dialog_id = inputs.scenario_id();
                                div()
                                    .on_children_prepainted(move |children, window, _| {
                                        if !scenario_enabled {
                                            return;
                                        }
                                        if let [bounds] = children.as_slice() {
                                            crate::ui_scenario::emit_ui_bounds(
                                                "connection-picker-next",
                                                "picker",
                                                dialog_id,
                                                bounds.clone(),
                                                enabled,
                                                window,
                                            );
                                        }
                                    })
                                    .child(next)
                            };
                            return vec![div()
                                .debug_selector(|| "connection-picker-footer".into())
                                .flex_1()
                                .mx(px(-18.0))
                                .px(px(18.0))
                                .h(px(60.0))
                                .flex()
                                .items_center()
                                .justify_between()
                                .border_t_1()
                                .border_color(theme::border())
                                .bg(theme::surface())
                                .child(inputs.picker_footer())
                                .child(next)
                                .into_any_element()];
                        }
                        let inputs_for_test = inputs.clone();
                        let initial_form_for_test = initial_form.clone();
                        let original_record_for_test = original_record.clone();
                        let root_for_test = root.clone();
                        let status = inputs.test_status.borrow().clone();
                        let running = status == crate::connection_dialog::TestStatus::Running;
                        let test_connection = Button::new("test-connection")
                            .label(if running { "Testing..." } else { "Test" })
                            .debug_selector(|| "test-connection".into())
                            .compact()
                            .outline()
                            .disabled(running)
                            .on_click(move |_, window, cx| {
                                let form =
                                    inputs_for_test.read_form(cx, initial_form_for_test.clone());
                                match form.validate() {
                                    Ok(()) => {
                                        let window_handle = window.window_handle();
                                        let status = inputs_for_test.test_status.clone();
                                        let _ = root_for_test.update(cx, |this, cx| {
                                            this.start_connection_test(
                                                form,
                                                original_record_for_test.clone(),
                                                window_handle,
                                                status,
                                                cx,
                                            );
                                        });
                                    }
                                    Err(error) => {
                                        *inputs_for_test.test_status.borrow_mut() =
                                            crate::connection_dialog::TestStatus::Failure(error);
                                    }
                                }
                                window.refresh();
                            });
                        let select_visible = Button::new("select-visible-databases")
                            .debug_selector(|| "select-visible-databases".into())
                            .icon(
                                gpui_component::Icon::empty()
                                    .path("icons/filter.svg")
                                    .size(px(14.0)),
                            )
                            .compact()
                            .outline()
                            .disabled(!inputs.saved.get())
                            .tooltip(if inputs.saved.get() {
                                "Select Visible Databases"
                            } else {
                                "Save the connection first to choose visible databases"
                            })
                            .on_click({
                                let root = root.clone();
                                let id = initial_form.id.map(|id| id.to_string());
                                move |_, window, cx| {
                                    let Some(id) = id.clone() else {
                                        return;
                                    };
                                    let _ = root.update(cx, |this, cx| {
                                        this.open_visible_filter_dialog(&id, window, cx);
                                    });
                                }
                            });
                        vec![div()
                            .debug_selector(|| "connection-config-footer".into())
                            .flex_1()
                            .mx(px(-20.0))
                            .px(px(20.0))
                            .h(px(56.0))
                            .flex()
                            .items_center()
                            .justify_end()
                            .gap(px(8.0))
                            .border_t_1()
                            .border_color(theme::border())
                            .child(test_status_label(&status))
                            .child(div().flex_shrink_0().child(select_visible))
                            .child(div().flex_shrink_0().child(test_connection))
                            .child(div().flex_shrink_0().child(ok(window, cx)))
                            .into_any_element()]
                    })(_window, _cx);
                    div().children(buttons).into_any_element()
                })
                .button_props(gpui_component::dialog::DialogButtonProps::default().ok_text(ok_text))
                .on_close({
                    let root_for_close = root.clone();
                    let inputs_for_close = inputs.clone();
                    move |_, window, cx| {
                        // The typed SSH password and passphrase do not outlive the dialog.
                        inputs_for_close.ssh.clear_secrets(window, cx);
                        if scenario_enabled {
                            #[cfg(feature = "ui-scenarios")]
                            crate::ui_scenario::emit_ui_dialog_closed(dialog_id, window);
                        }
                        let _ = root_for_close.update(cx, |this, cx| {
                            this.invalidate_connection_test();
                            cx.notify();
                        });
                    }
                })
                .on_ok({
                    let input_state = inputs.clone();
                    let initial_form = form.clone();
                    let original_record = original_record.clone();
                    let root_for_ok = root.clone();
                    let scenario_enabled = scenario_enabled;
                    move |_, window, cx| {
                        if input_state.is_picker() {
                            if !input_state.picker_selection_is_visible() {
                                return false;
                            }
                            input_state.set_step(
                                crate::connection_dialog::ConnectionDialogStep::Config,
                                cx,
                            );
                            return false;
                        }
                        let scenario_result = root_for_ok.update(cx, |this, cx| {
                            this.scenario_ui.service.as_ref()?;
                            Some(this.save_scenario_form(
                                &input_state,
                                initial_form.clone(),
                                require_password,
                                if require_password {
                                    "save-and-connect"
                                } else {
                                    "save"
                                },
                                cx,
                            ))
                        });
                        if scenario_enabled {
                            match scenario_result {
                                Ok(Some(Ok(form))) => {
                                    let _ = root_for_ok.update(cx, |this, _| {
                                        this.invalidate_connection_test();
                                    });
                                    if require_password {
                                        if let Some(connection) = form.id {
                                            let _ = root_for_ok.update(cx, |this, cx| {
                                                this.start_scenario_connect(
                                                    connection.to_string(),
                                                    form.name.clone(),
                                                    None,
                                                    cx,
                                                );
                                            });
                                        }
                                    }
                                    return true;
                                }
                                Ok(Some(Err(error))) => {
                                    window.open_alert_dialog(cx, move |dialog, _, _| {
                                        dialog
                                            .title("Could not save connection")
                                            .child(error.clone())
                                    });
                                }
                                _ => {}
                            }
                            return false;
                        }
                        let result = read_and_save_dialog_form(
                            &input_state,
                            initial_form.clone(),
                            original_record.as_ref(),
                            require_password,
                            cx,
                        );
                        match result {
                            Ok(form) => {
                                let runtime_uri = if require_password {
                                    match runtime_uri_for_form(&form, original_record.as_ref()) {
                                        Ok(uri) => Some(uri),
                                        Err(error) => {
                                            let _ = root_for_ok.update(cx, |this, _| {
                                                this.report_error(Area::Connect, None, &error)
                                            });
                                            window.open_alert_dialog(cx, move |dialog, _, _| {
                                                dialog
                                                    .title("Invalid connection URI")
                                                    .child(error.clone())
                                            });
                                            return false;
                                        }
                                    }
                                } else {
                                    None
                                };
                                // What the sessions of its tabs connect with changed: they reconnect.
                                let reconnect = original_record.as_ref().and_then(|original| {
                                    let saved = form.clone().to_record(current_time(), Some(original)).ok()?;
                                    let typed = !form.password.is_empty() || !form.ssh_secrets.secrets(&form.ssh.clone().unwrap_or_default()).is_empty();
                                    (typed
                                        || saved.uri != original.uri
                                        || saved.db_type != original.db_type
                                        || saved.network_settings_json != original.network_settings_json)
                                        .then_some(())
                                });
                                let _ = root_for_ok.update(cx, |this, cx| {
                                    this.refresh_saved_connections(cx);
                                    if let (Some(id), Some(())) = (form.id, reconnect) {
                                        this.connection_edited(&id.to_string(), cx);
                                    }
                                    if let Some(id) = form.id {
                                        // An agent session opened on the old settings does not survive the edit.
                                        this.revoke_agent_access(&id.to_string());
                                        // What was typed replaces whatever the old settings held.
                                        let key = id.to_string();
                                        this.tunnel_ui.secrets.remove(&key);
                                        let typed = form.ssh_secrets.secrets(&form.ssh.clone().unwrap_or_default());
                                        if !typed.is_empty() {
                                            this.tunnel_ui.secrets.insert(key, typed);
                                        }
                                    }
                                    if require_password {
                                        if let (Some(id), Some(runtime_uri)) =
                                            (form.id, runtime_uri)
                                        {
                                            this.ensure_connected_with_uri(
                                                id.to_string(),
                                                Some(runtime_uri),
                                                None,
                                                cx,
                                            );
                                        }
                                    }
                                    cx.notify();
                                });
                                true
                            }
                            Err(error) => {
                                let _ = root_for_ok.update(cx, |this, _| {
                                    this.report_error(Area::Connect, None, &error)
                                });
                                window.open_alert_dialog(cx, move |dialog, _, _| {
                                    dialog
                                        .title("Could not save connection")
                                        .child(error.clone())
                                });
                                false
                            }
                        }
                    }
                })
        });
        cx.notify();
    }

    pub(super) fn invalidate_connection_test(&mut self) {
        self.connection_ui.test_generation = self.connection_ui.test_generation.wrapping_add(1);
        self.connection_ui.test_in_flight.set(false);
    }

    #[cfg(test)]
    fn record_scenario_test_terminal(&mut self) {
        self.scenario_ui.test_terminal_count += 1;
    }

    #[cfg(not(test))]
    fn record_scenario_test_terminal(&mut self) {}
}
